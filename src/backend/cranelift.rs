use anyhow::Result;
use cranelift_codegen::ir::{
    AbiParam, InstBuilder, MemFlags, StackSlotData, StackSlotKind, TrapCode, UserFuncName,
    condcodes::{FloatCC, IntCC},
    types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::collections::{HashMap, HashSet};

use crate::backend::abi;
use crate::module::std_registry;
use crate::parser::ast::*;
use crate::semantic::symbols::{EnumInfo, EnumVariantInfo, InterfaceInfo};
use crate::{BuildMode, CodegenOptions};

const USER_MAIN_SYMBOL: &str = "willow_user_main";
/// Generated function that initializes all `static` properties before `main`
/// (willow-qsqf §13.5).
const STATIC_INIT_SYMBOL: &str = "__willow_static_init";

#[derive(Debug, Clone)]
struct ParamDebug {
    name: String,
    ty: Type,
    mode: ParamMode,
}

#[derive(Default)]
struct ModuleAliasSnapshot {
    func_ids: Vec<(String, Option<FuncId>)>,
    func_return_types: Vec<(String, Option<Type>)>,
    fn_types: Vec<(String, Option<Type>)>,
    func_param_modes: Vec<(String, Option<Vec<ParamMode>>)>,
    func_param_debug: Vec<(String, Option<Vec<ParamDebug>>)>,
    class_layouts: Vec<(String, Option<Vec<(String, Type)>>)>,
    class_base: Vec<(String, Option<String>)>,
    class_type_ids: Vec<(String, Option<i64>)>,
}

fn insert_with_snapshot<T: Clone>(
    snapshots: &mut Vec<(String, Option<T>)>,
    map: &mut HashMap<String, T>,
    key: String,
    value: T,
) {
    let old = map.insert(key.clone(), value);
    snapshots.push((key, old));
}

fn restore_snapshots<T>(map: &mut HashMap<String, T>, snapshots: Vec<(String, Option<T>)>) {
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

pub struct Codegen {
    module: ObjectModule,
    func_ids: HashMap<String, FuncId>,
    func_return_types: HashMap<String, Type>,
    /// Full `Type::Fn(params, ret)` for each declared function — used to type function values.
    fn_types: HashMap<String, Type>,
    /// Parameter passing modes for declared Willow functions, keyed like `func_ids`.
    func_param_modes: HashMap<String, Vec<ParamMode>>,
    /// Source-level parameter names/types/modes for debug reference-call hooks.
    func_param_debug: HashMap<String, Vec<ParamDebug>>,
    /// Imported module access name -> canonical symbol prefix.
    known_modules: HashMap<String, String>,
    /// Maps each lambda's source span to its generated private function name.
    lambda_names: HashMap<crate::diagnostics::Span, String>,
    /// Counter for generating unique lambda names.
    lambda_counter: usize,
    /// Maps each spawn expression's source span to its generated trampoline name.
    spawn_tramp_names: HashMap<crate::diagnostics::Span, String>,
    /// Resolved `(param_types, return_type)` for function-pointer spawn sites
    /// (callee is a local `Fn` value, not a named function), keyed by the spawn
    /// expression's span. Populated from the type checker; lets the backend
    /// compile a cooperative `call_indirect` poll trampoline so the task is
    /// scheduled instead of run inline (willow-spawn-fptr).
    spawn_fptr_sigs: HashMap<crate::diagnostics::Span, (Vec<Type>, Type)>,
    /// Source names of async fns lowered as cooperative leaf tasks (constructor +
    /// poll fn). `await f()` of such a fn block-runs the scheduler and reads the
    /// frame's result slot (willow-lpn.5.3 slice 4).
    cooperative_leaves: std::collections::HashSet<String>,
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
    /// Resolved types of async-fn `let` locals (keyed by span) so the backend
    /// can frame-back unannotated live-across-await locals (willow-lpn.5c).
    async_local_types: HashMap<crate::diagnostics::Span, Type>,
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
    pub fn new(opts: &CodegenOptions) -> Result<Self> {
        let isa_builder = cranelift_native::builder().map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut flag_builder = settings::builder();
        match opts.build_mode {
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
            func_ids: HashMap::new(),
            func_return_types: HashMap::new(),
            fn_types: HashMap::new(),
            func_param_modes: HashMap::new(),
            func_param_debug: HashMap::new(),
            known_modules: HashMap::new(),
            lambda_names: HashMap::new(),
            lambda_counter: 0,
            spawn_tramp_names: HashMap::new(),
            spawn_fptr_sigs: HashMap::new(),
            cooperative_leaves: std::collections::HashSet::new(),
            string_literals: HashMap::new(),
            string_counter: 0,
            runtime_declared: false,
            class_layouts: HashMap::new(),
            build_mode: opts.build_mode,
            source_file: String::new(),
            enum_infos: HashMap::new(),
            class_base: HashMap::new(),
            class_type_ids: HashMap::new(),
            lambda_return_types: HashMap::new(),
            async_local_types: HashMap::new(),
            interface_infos: HashMap::new(),
            vtable_ids: HashMap::new(),
            static_storage: HashMap::new(),
            static_init_order: Vec::new(),
        })
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
    pub fn register_async_local_types(&mut self, types: HashMap<crate::diagnostics::Span, Type>) {
        self.async_local_types = types;
    }

    /// Register resolved signatures of function-pointer spawn sites so the backend
    /// can compile cooperative poll trampolines for them (willow-spawn-fptr).
    pub fn register_spawn_fptr_sigs(
        &mut self,
        sigs: HashMap<crate::diagnostics::Span, (Vec<Type>, Type)>,
    ) {
        self.spawn_fptr_sigs = sigs;
    }

    /// Register the type-checker-inferred return types for all lambdas in the program.
    /// Must be called before compile_program / compile_module so that declare_lambda
    /// can emit correct signatures for unannotated lambdas.
    pub fn register_lambda_return_types(&mut self, types: HashMap<crate::diagnostics::Span, Type>) {
        self.lambda_return_types = types;
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
            self.func_ids.insert(local.to_string(), id);
            if let Some(rt) = self.func_return_types.get(&mangled).cloned() {
                self.func_return_types.insert(local.to_string(), rt);
            }
            if let Some(ft) = self.fn_types.get(&mangled).cloned() {
                self.fn_types.insert(local.to_string(), ft);
            }
            if let Some(modes) = self.func_param_modes.get(&mangled).cloned() {
                self.func_param_modes.insert(local.to_string(), modes);
            }
            if let Some(params) = self.func_param_debug.get(&mangled).cloned() {
                self.func_param_debug.insert(local.to_string(), params);
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
                .keys()
                .filter(|k| k.starts_with(&method_prefix))
                .cloned()
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
            insert_with_snapshot(
                &mut aliases.func_ids,
                &mut self.func_ids,
                alias.to_string(),
                id,
            );
        }
        if let Some(ret) = self.func_return_types.get(canonical).cloned() {
            insert_with_snapshot(
                &mut aliases.func_return_types,
                &mut self.func_return_types,
                alias.to_string(),
                ret,
            );
        }
        if let Some(ty) = self.fn_types.get(canonical).cloned() {
            insert_with_snapshot(
                &mut aliases.fn_types,
                &mut self.fn_types,
                alias.to_string(),
                ty,
            );
        }
        if let Some(modes) = self.func_param_modes.get(canonical).cloned() {
            insert_with_snapshot(
                &mut aliases.func_param_modes,
                &mut self.func_param_modes,
                alias.to_string(),
                modes,
            );
        }
        if let Some(params) = self.func_param_debug.get(canonical).cloned() {
            insert_with_snapshot(
                &mut aliases.func_param_debug,
                &mut self.func_param_debug,
                alias.to_string(),
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
    }

    fn restore_module_aliases(&mut self, aliases: ModuleAliasSnapshot) {
        restore_snapshots(&mut self.func_ids, aliases.func_ids);
        restore_snapshots(&mut self.func_return_types, aliases.func_return_types);
        restore_snapshots(&mut self.fn_types, aliases.fn_types);
        restore_snapshots(&mut self.func_param_modes, aliases.func_param_modes);
        restore_snapshots(&mut self.func_param_debug, aliases.func_param_debug);
        restore_snapshots(&mut self.class_layouts, aliases.class_layouts);
        restore_snapshots(&mut self.class_base, aliases.class_base);
        restore_snapshots(&mut self.class_type_ids, aliases.class_type_ids);
    }

    fn class_method_symbol(&self, class_name: &str, method_name: &str) -> String {
        class_method_symbol_name(&self.known_modules, class_name, method_name)
    }

    /// Compile an imported module. Functions are given the mangled name
    /// `{canonical_module_path}__{fn}` with `::` normalized to `__`.
    /// Must be called before `compile_program` so the entry module can call them.
    pub fn compile_module(
        &mut self,
        mod_name: &str,
        canonical_path: &str,
        program: &Program,
        source_file: &str,
    ) -> Result<()> {
        let normalized_program = normalize_std_collection_program(program);
        let program = &normalized_program;
        self.source_file = source_file.to_string();
        let module_prefix = module_symbol_prefix(canonical_path);
        self.known_modules
            .insert(mod_name.to_string(), module_prefix.clone());
        self.declare_runtime()?;
        self.declare_string_literals(program)?;
        if self.build_mode == BuildMode::Debug {
            self.declare_string_literal(source_file)?;
            for name in collect_nil_check_names(program) {
                self.declare_string_literal(&name)?;
            }
            self.declare_reference_debug_strings(program)?;
        }

        let module_classes: Vec<(String, ClassDecl)> = program
            .items
            .iter()
            .filter_map(|item| {
                let Item::Class(c) = item else {
                    return None;
                };
                let local_name = c.name.clone();
                let qualified = qualify_module_class_decl(c, mod_name);
                Some((local_name, qualified))
            })
            .collect();

        // Register imported module class layouts and methods under their
        // module-qualified names so entry code can call `geom::Point::new(...)`.
        for (_, c) in &module_classes {
            let fields: Vec<(String, Type)> = c
                .fields
                .iter()
                .map(|f| (f.name.clone(), f.ty.clone()))
                .collect();
            self.class_layouts.insert(c.name.clone(), fields);
        }
        for (_, c) in &module_classes {
            self.register_class_layout(c);
            self.declare_class_methods(c)?;
            // Static-property storage for imported modules (replayed by
            // `__willow_static_init`, compiled in the entry's compile_program).
            self.declare_static_storage_for_class(&c.name, c)?;
        }

        // Forward-declare all functions in this module.
        for item in &program.items {
            match item {
                Item::Function(f) => {
                    let mangled = format!("{}__{}", module_prefix, f.name);
                    self.declare_function_named(&mangled, f)?;
                }
                Item::Enum(_) | Item::Class(_) | Item::Interface(_) => {}
            }
        }

        // Emit (class, interface) vtables for module classes that implement an
        // interface (their methods are declared above; implements paths were
        // module-qualified by `qualify_module_class_decl`).
        let qualified_classes: Vec<ClassDecl> =
            module_classes.iter().map(|(_, c)| c.clone()).collect();
        self.declare_vtables_for_classes(&qualified_classes)?;

        let mut aliases = ModuleAliasSnapshot::default();
        for item in &program.items {
            if let Item::Function(f) = item {
                let mangled = format!("{}__{}", module_prefix, f.name);
                self.alias_function_symbol(&f.name, &mangled, &mut aliases);
            }
        }
        for (local_name, qualified) in &module_classes {
            self.alias_class_symbol(local_name, &qualified.name, &mut aliases);
            for method in &qualified.methods {
                let local_mangled = format!("{}__{}", local_name, method.name);
                let qualified_mangled = self.class_method_symbol(&qualified.name, &method.name);
                self.alias_function_symbol(&local_mangled, &qualified_mangled, &mut aliases);
            }
        }

        let result = (|| -> Result<()> {
            // Collect spawn sites and declare/compile trampolines. Named-function
            // spawns get a direct-call trampoline; function-pointer spawns get a
            // `call_indirect` trampoline keyed by the type-checker-resolved
            // signature (willow-spawn-fptr).
            let all_spawns = collect_spawns_in_program(program);
            let spawns: Vec<_> = all_spawns
                .iter()
                .filter(|(_, _, callee)| self.func_ids.contains_key(callee))
                .cloned()
                .collect();
            let fptr_spawns: Vec<_> = all_spawns
                .iter()
                .filter(|(span, _, callee)| {
                    !self.func_ids.contains_key(callee) && self.spawn_fptr_sigs.contains_key(span)
                })
                .cloned()
                .collect();
            for (span, tramp_name, _callee) in &spawns {
                self.spawn_tramp_names.insert(*span, tramp_name.clone());
                self.declare_spawn_trampoline(tramp_name)?;
            }
            for (_span, tramp_name, callee) in &spawns {
                let mangled_callee = format!("{}__{}", module_prefix, callee);
                self.compile_spawn_trampoline(tramp_name, &mangled_callee)?;
            }
            for (span, tramp_name, _callee) in &fptr_spawns {
                self.spawn_tramp_names.insert(*span, tramp_name.clone());
                self.declare_spawn_trampoline(tramp_name)?;
            }
            for (span, tramp_name, _callee) in &fptr_spawns {
                self.compile_spawn_fptr_trampoline(tramp_name, *span)?;
            }

            // Compile bodies.
            for item in &program.items {
                match item {
                    Item::Function(f) => {
                        let mangled = format!("{}__{}", module_prefix, f.name);
                        self.compile_function_named(&mangled, f)?;
                    }
                    Item::Class(_) | Item::Enum(_) | Item::Interface(_) => {}
                }
            }
            for (_, c) in &module_classes {
                self.compile_class_methods(c)?;
            }
            Ok(())
        })();

        self.restore_module_aliases(aliases);
        result
    }

    pub fn compile_program(&mut self, program: &Program, source_file: &str) -> Result<()> {
        let normalized_program = normalize_std_collection_program(program);
        let program = &normalized_program;
        self.source_file = source_file.to_string();
        self.declare_runtime()?;
        self.declare_string_literals(program)?;
        if self.build_mode == BuildMode::Debug {
            self.declare_string_literal(source_file)?;
            for name in collect_nil_check_names(program) {
                self.declare_string_literal(&name)?;
            }
            self.declare_reference_debug_strings(program)?;
        }

        // Register class layouts in two passes so that base-class layouts are available
        // when derived-class layouts are built (handles any declaration order).
        // Pass 1: register layouts without inherited fields (direct fields only).
        for item in &program.items {
            if let Item::Class(c) = item {
                let fields: Vec<(String, Type)> = c
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), f.ty.clone()))
                    .collect();
                self.class_layouts.insert(c.name.clone(), fields);
            }
        }
        // Pass 2: rebuild layouts to prepend inherited fields, then forward-declare methods.
        for item in &program.items {
            match item {
                Item::Class(c) => {
                    self.register_class_layout(c);
                    self.declare_class_methods(c)?;
                    self.declare_static_storage_for_class(&c.name, c)?;
                }
                Item::Enum(_) => {} // enum infos are registered via register_enum_info before compile
                _ => {}
            }
        }

        // Identify async fns lowered as cooperative leaf tasks (willow-lpn.5.3).
        // A fn is a cooperative leaf iff its body is leaf-lowerable AND every
        // async fn it awaits is itself a cooperative leaf, so this is a fixpoint:
        // sleep-only leaves are found first, then fns whose call-awaits target
        // them, and so on (willow-lpn.5.3.1).
        loop {
            let mut changed = false;
            for item in &program.items {
                if let Item::Function(f) = item {
                    if !self.cooperative_leaves.contains(&f.name)
                        && cooperative_leaf_eligible(
                            f,
                            &self.async_local_types,
                            &self.cooperative_leaves,
                        )
                    {
                        self.cooperative_leaves.insert(f.name.clone());
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }

        // Forward-declare all user functions first
        for item in &program.items {
            match item {
                Item::Function(f) => self.declare_user_function(f)?,
                Item::Class(_) | Item::Enum(_) | Item::Interface(_) => {}
            }
        }

        // Emit one static vtable per (class, implemented-interface) pair. All
        // class method symbols are declared by now, so the vtable can reference
        // them by function address.
        self.declare_interface_vtables(program)?;

        // Collect and declare all lambdas (they may call user functions already declared above).
        let lambdas = collect_lambdas_in_program(program);
        for (name, lambda) in &lambdas {
            self.declare_lambda(name, lambda)?;
            self.lambda_names.insert(lambda.span, name.clone());
        }

        // Compile lambdas first (user functions are already declared, so calls inside work).
        for (name, lambda) in &lambdas {
            self.compile_lambda(name, lambda)?;
        }

        // Collect spawn sites and declare/compile trampolines.
        // Must happen after all function declarations so fn_types is populated.
        // Named-function spawns get a direct-call trampoline; function-pointer
        // spawns get a `call_indirect` trampoline keyed by the resolved signature
        // (willow-spawn-fptr).
        let all_spawns = collect_spawns_in_program(program);
        let spawns: Vec<_> = all_spawns
            .iter()
            .filter(|(_, _, callee)| self.func_ids.contains_key(callee))
            .cloned()
            .collect();
        let fptr_spawns: Vec<_> = all_spawns
            .iter()
            .filter(|(span, _, callee)| {
                !self.func_ids.contains_key(callee) && self.spawn_fptr_sigs.contains_key(span)
            })
            .cloned()
            .collect();
        for (span, tramp_name, _callee) in &spawns {
            self.spawn_tramp_names.insert(*span, tramp_name.clone());
            self.declare_spawn_trampoline(tramp_name)?;
        }
        for (_span, tramp_name, callee) in &spawns {
            self.compile_spawn_trampoline(tramp_name, callee)?;
        }
        for (span, tramp_name, _callee) in &fptr_spawns {
            self.spawn_tramp_names.insert(*span, tramp_name.clone());
            self.declare_spawn_trampoline(tramp_name)?;
        }
        for (span, tramp_name, _callee) in &fptr_spawns {
            self.compile_spawn_fptr_trampoline(tramp_name, *span)?;
        }

        // Always declare `__willow_static_init` (willow-qsqf §13.5). The runtime
        // calls it after `gc_init` and before `willow_user_main`; it is a no-op
        // when the program has no static properties. Declaring it unconditionally
        // keeps the runtime call path uniform regardless of the `main` lowering.
        self.declare_static_init()?;

        // Compile user function bodies and class methods
        for item in &program.items {
            match item {
                Item::Function(f) => self.compile_function(f)?,
                Item::Class(c) => self.compile_class_methods(c)?,
                Item::Enum(_) => {} // no codegen needed for enum declarations
                Item::Interface(_) => {} // interfaces emit no code in Stage 1 (vtables: Stage 3)
            }
        }

        // Compile the static-init function body after all symbols are defined.
        self.compile_static_init()?;
        Ok(())
    }

    /// Declare the `__willow_static_init` symbol (no params, no returns). Exported
    /// so the runtime entry can call it before `main` (willow-qsqf §13.5).
    fn declare_static_init(&mut self) -> Result<()> {
        if self.func_ids.contains_key(STATIC_INIT_SYMBOL) {
            return Ok(());
        }
        let sig = self.module.make_signature();
        let id = self
            .module
            .declare_function(STATIC_INIT_SYMBOL, Linkage::Export, &sig)?;
        self.func_ids.insert(STATIC_INIT_SYMBOL.to_string(), id);
        Ok(())
    }

    /// Declare the signature for a lambda private function.
    fn declare_lambda(&mut self, name: &str, l: &LambdaExpr) -> Result<()> {
        let mut sig = self.module.make_signature();
        for p in &l.params {
            let ty = p.ty.as_ref().map(clif_type).unwrap_or(types::I64);
            sig.params.push(AbiParam::new(ty));
        }
        // Prefer explicit annotation, then type-checker inferred type, then I64 fallback.
        let ast_ret = l
            .return_type
            .clone()
            .or_else(|| self.lambda_return_types.get(&l.span).cloned())
            .unwrap_or(Type::I64);
        sig.returns.push(AbiParam::new(clif_type(&ast_ret)));
        let id = self.module.declare_function(name, Linkage::Local, &sig)?;
        self.func_ids.insert(name.to_string(), id);
        self.func_return_types
            .insert(name.to_string(), ast_ret.clone());
        self.func_param_modes.insert(
            name.to_string(),
            l.params.iter().map(|_| ParamMode::Value).collect(),
        );
        self.func_param_debug.insert(
            name.to_string(),
            l.params
                .iter()
                .map(|p| ParamDebug {
                    name: p.name.clone(),
                    ty: p.ty.clone().unwrap_or(Type::I64),
                    mode: ParamMode::Value,
                })
                .collect(),
        );
        let param_types: Vec<Type> = l.params.iter().filter_map(|p| p.ty.clone()).collect();
        self.fn_types
            .insert(name.to_string(), Type::Fn(param_types, Box::new(ast_ret)));
        Ok(())
    }

    /// Compile a lambda as a private function.
    fn compile_lambda(&mut self, name: &str, l: &LambdaExpr) -> Result<()> {
        let params: Vec<Param> = l
            .params
            .iter()
            .filter_map(|p| {
                p.ty.as_ref().map(|ty| Param {
                    name: p.name.clone(),
                    ty: ty.clone(),
                    mode: ParamMode::Value,
                    span: p.span,
                    type_span: p.span,
                })
            })
            .collect();
        let return_type = l
            .return_type
            .clone()
            .or_else(|| self.lambda_return_types.get(&l.span).cloned())
            .unwrap_or(Type::I64);
        let body = match &l.body {
            LambdaBody::Block(b) => b.clone(),
            LambdaBody::Expr(e) => Block {
                stmts: vec![Stmt::Return(ReturnStmt {
                    value: Some(*e.clone()),
                    span: e.span(),
                })],
                span: l.span,
            },
        };
        let f = FunctionDecl {
            name: name.to_string(),
            public: false,
            is_async: false,
            params,
            return_type,
            body,
            span: l.span,
        };
        self.compile_function_named(name, &f)
    }

    /// Declare the signature for a spawn trampoline: fn(data_ptr: I64) -> void.
    fn declare_spawn_trampoline(&mut self, name: &str) -> Result<()> {
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        let id = self.module.declare_function(name, Linkage::Local, &sig)?;
        self.func_ids.insert(name.to_string(), id);
        Ok(())
    }

    /// Compile a spawn trampoline for the given callee.
    ///
    /// Layout of the data area (pointed to by data_ptr):
    ///   offset  0      : result slot (8 bytes, or 0 bytes for void)
    ///   offset  result_slot_size       : arg0 (8 bytes)
    ///   offset  result_slot_size + 8   : arg1 (8 bytes)
    ///   ...
    /// Compile the cooperative POLL fn for a `spawn f(args)` task. The spawned
    /// task runs on the single-threaded cooperative scheduler (not an OS thread).
    /// The frame is a GC async-frame whose slot 0 holds the result and slots 1..
    /// hold the args; this poll fn loads the args, calls `f` to completion, stores
    /// the result in slot 0, and returns Ready (a synchronous callee finishes in a
    /// single poll). `join` later drives the scheduler and reads slot 0.
    fn compile_spawn_trampoline(&mut self, tramp_name: &str, callee: &str) -> Result<()> {
        let func_id = self.func_ids[tramp_name];

        let Some(fn_ty) = self.fn_types.get(callee).cloned() else {
            return Ok(());
        };
        let (param_types, ret_type) = match &fn_ty {
            Type::Fn(p, r) => (p.clone(), *r.clone()),
            _ => return Ok(()),
        };

        // Poll fn signature: (frame: i64) -> i32 (RuntimePollFn; 1 = Ready).
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I32));
        let callee_fid = self.func_ids[callee];

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());

        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);

        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let frame = builder.block_params(entry)[0];

        // Args live in frame slots 1.. (slot 0 is reserved for the result).
        let mut call_args = Vec::new();
        for (i, pt) in param_types.iter().enumerate() {
            let offset = async_frame_slot_offset(1 + i);
            let val = builder
                .ins()
                .load(clif_type(pt), MemFlags::new(), frame, offset);
            call_args.push(val);
        }

        // Call the target function to completion.
        let callee_fref = self.module.declare_func_in_func(callee_fid, builder.func);
        let call = builder.ins().call(callee_fref, &call_args);
        let results = builder.inst_results(call).to_vec();

        // Store the result (if non-void) into frame slot 0.
        if ret_type != Type::Void {
            let result_val = results[0];
            let result_off = async_frame_slot_offset(0);
            builder
                .ins()
                .store(MemFlags::new(), result_val, frame, result_off);
        }

        // Synchronous callee → the task is Ready after one poll.
        let ready = builder.ins().iconst(types::I32, 1);
        builder.ins().return_(&[ready]);
        builder.finalize();

        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// Compile the cooperative POLL fn for a function-pointer `spawn fptr(args)`
    /// task (willow-spawn-fptr). Unlike `compile_spawn_trampoline`, the callee is
    /// not a named function but a function value stored in the async frame, so the
    /// poll fn loads it and uses `call_indirect`.
    ///
    /// Frame layout: slot 0 = result, slot 1 = function pointer, slots 2.. = args.
    /// (The fptr is a raw code address, never a GC reference, so its slot is not
    /// marked in the frame's GC mask.) The callee is synchronous, so the task is
    /// Ready after a single poll; `join` later reads slot 0.
    fn compile_spawn_fptr_trampoline(
        &mut self,
        tramp_name: &str,
        span: crate::diagnostics::Span,
    ) -> Result<()> {
        let func_id = self.func_ids[tramp_name];
        let Some((param_types, ret_type)) = self.spawn_fptr_sigs.get(&span).cloned() else {
            return Ok(());
        };

        // Poll fn signature: (frame: i64) -> i32 (RuntimePollFn; 1 = Ready).
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I32));

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());

        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);

        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let frame = builder.block_params(entry)[0];

        // The function pointer lives in frame slot 1; args follow in slots 2..
        let fptr_off = async_frame_slot_offset(1);
        let fptr = builder
            .ins()
            .load(types::I64, MemFlags::new(), frame, fptr_off);

        let mut call_args = Vec::new();
        for (i, pt) in param_types.iter().enumerate() {
            let offset = async_frame_slot_offset(2 + i);
            let val = builder
                .ins()
                .load(clif_type(pt), MemFlags::new(), frame, offset);
            call_args.push(val);
        }

        // Build the indirect call signature from the resolved fn type.
        let mut call_sig = self.module.make_signature();
        for pt in &param_types {
            call_sig.params.push(AbiParam::new(clif_type(pt)));
        }
        if ret_type != Type::Void {
            call_sig.returns.push(AbiParam::new(clif_type(&ret_type)));
        }
        let sig_ref = builder.import_signature(call_sig);
        let call = builder.ins().call_indirect(sig_ref, fptr, &call_args);
        let results = builder.inst_results(call).to_vec();

        // Store the result (if non-void) into frame slot 0.
        if ret_type != Type::Void {
            let result_val = results[0];
            let result_off = async_frame_slot_offset(0);
            builder
                .ins()
                .store(MemFlags::new(), result_val, frame, result_off);
        }

        // Synchronous callee → the task is Ready after one poll.
        let ready = builder.ins().iconst(types::I32, 1);
        builder.ins().return_(&[ready]);
        builder.finalize();

        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    fn declare_runtime(&mut self) -> Result<()> {
        if self.runtime_declared {
            return Ok(());
        }

        // The runtime ABI surface is declared from a single source of truth in
        // `crate::backend::abi`. Adding or changing a runtime symbol means
        // editing `RUNTIME_SYMBOLS`, not this loop.
        let ptr_ty = self.module.target_config().pointer_type();
        for symbol in abi::RUNTIME_SYMBOLS {
            let mut sig = self.module.make_signature();
            symbol.fill_signature(&mut sig, ptr_ty);
            let id = self
                .module
                .declare_function(symbol.name, Linkage::Import, &sig)?;
            self.func_ids.insert(symbol.name.to_string(), id);
        }
        self.runtime_declared = true;
        Ok(())
    }

    fn declare_string_literals(&mut self, program: &Program) -> Result<()> {
        for value in collect_string_literals_in_program(program) {
            self.declare_string_literal(&value)?;
        }
        // Pre-declare builtin panic messages used by Option/Result helper methods.
        for msg in [
            "called `Option::unwrap()` on a `None` value",
            "called `Result::unwrap()` on an `Err` value",
            "called `Result::unwrap_err()` on an `Ok` value",
            "interface downcast box",
            "interface downcast object",
        ] {
            self.declare_string_literal(msg)?;
        }
        Ok(())
    }

    fn declare_reference_debug_strings(&mut self, program: &Program) -> Result<()> {
        for value in collect_reference_debug_strings_in_program(program) {
            self.declare_string_literal(&value)?;
        }
        Ok(())
    }

    fn declare_string_literal(&mut self, value: &str) -> Result<()> {
        if self.string_literals.contains_key(value) {
            return Ok(());
        }

        let name = format!("__willow_str_{}", self.string_counter);
        self.string_counter += 1;
        let data_id = self
            .module
            .declare_data(&name, Linkage::Local, false, false)?;
        let mut data = DataDescription::new();
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        data.define(bytes.into_boxed_slice());
        self.module.define_data(data_id, &data)?;
        self.string_literals.insert(value.to_string(), data_id);
        Ok(())
    }

    fn declare_user_function(&mut self, f: &FunctionDecl) -> Result<()> {
        let symbol_name = user_function_symbol(&f.name);
        self.declare_function_symbol(&f.name, &symbol_name, f, f.name == "main")
    }

    fn declare_function_named(&mut self, name: &str, f: &FunctionDecl) -> Result<()> {
        self.declare_function_symbol(name, name, f, false)
    }

    fn declare_function_symbol(
        &mut self,
        lookup_name: &str,
        symbol_name: &str,
        f: &FunctionDecl,
        export: bool,
    ) -> Result<()> {
        let mut sig = self.module.make_signature();
        let ptr_ty = self.module.target_config().pointer_type();
        // `willow_user_main` is parameterless even when `fn main(args:
        // Array<String>)` is declared (see compile_function_named).
        if symbol_name != USER_MAIN_SYMBOL {
            for param in &f.params {
                sig.params
                    .push(AbiParam::new(param_abi_type(param, ptr_ty)));
            }
        }
        let call_return_type = function_call_return_type(f);
        // A `Result<void, E>` main lowers to a VOID `willow_user_main` (it
        // inspects its result and exits in the body; willow-exg). Keep this in
        // sync with compile_function_named.
        let force_void_main = symbol_name == USER_MAIN_SYMBOL && main_result_err_type(f).is_some();
        if call_return_type != Type::Void && !force_void_main {
            sig.returns
                .push(AbiParam::new(clif_type(&call_return_type)));
        }
        let linkage = if export {
            Linkage::Export
        } else {
            Linkage::Local
        };
        let id = self.module.declare_function(symbol_name, linkage, &sig)?;
        self.func_ids.insert(lookup_name.to_string(), id);
        self.func_return_types
            .insert(lookup_name.to_string(), call_return_type.clone());
        self.func_param_modes.insert(
            lookup_name.to_string(),
            f.params.iter().map(|p| p.mode.clone()).collect(),
        );
        self.func_param_debug
            .insert(lookup_name.to_string(), param_debug_from_params(&f.params));
        // Store full function type for use when the function is passed as a value.
        let param_types = f.params.iter().map(|p| p.ty.clone()).collect();
        self.fn_types.insert(
            lookup_name.to_string(),
            Type::Fn(param_types, Box::new(call_return_type)),
        );
        Ok(())
    }

    fn compile_function(&mut self, f: &FunctionDecl) -> Result<()> {
        self.compile_function_named(&f.name.clone(), f)
    }

    /// Cooperative-async lowering (willow-lpn.5.3, Stage 2 — first slice):
    /// compile an eligible `async fn main` as a SUSPENDING poll function driven
    /// by the cooperative scheduler. `willow_user_main` becomes a driver that
    /// allocates the frame, spawns the poll fn as a task, and runs the scheduler;
    /// the poll fn is a state machine whose `await sleep(n)` points store the
    /// next state in the frame's state word (offset 0), call `willow_sched_sleep`,
    /// and return Pending — the timer-aware run loop resumes it. Only the strict
    /// shape accepted by `cooperative_main_eligible` takes this path; every other
    /// async fn keeps the existing eager lowering.
    fn compile_cooperative_main(&mut self, name: &str, f: &FunctionDecl) -> Result<()> {
        // Declare the poll fn `fn(frame: i64) -> i32`.
        let poll_symbol = format!("{}__poll", USER_MAIN_SYMBOL);
        let mut poll_sig = self.module.make_signature();
        poll_sig.params.push(AbiParam::new(types::I64));
        poll_sig.returns.push(AbiParam::new(types::I32));
        let poll_fid = self
            .module
            .declare_function(&poll_symbol, Linkage::Local, &poll_sig)?;
        self.func_ids.insert(poll_symbol.clone(), poll_fid);

        // Frame layout: slots for GC-managed locals (frame-backed across awaits).
        let layout = self.coop_frame_layout(&f.body);
        // Frame-back EVERY local (GC and non-GC) so it survives suspension; only
        // GC-managed slots are in `gc_slot_mask` (traced), so non-GC slots hold
        // plain scalars (willow-lpn.5.3 slice 3b).
        let mut offsets: HashMap<crate::diagnostics::Span, i32> = HashMap::new();
        for (i, slot) in layout.slots.iter().enumerate() {
            offsets.insert(slot.key, async_frame_slot_offset(i));
        }
        let slot_count = layout.slot_count() as i64;
        let mask = layout.gc_slot_mask as i64;

        self.compile_coop_main_driver(name, &poll_symbol, slot_count, mask)?;
        self.compile_coop_main_poll(&poll_symbol, f, offsets, None, &[])?;
        Ok(())
    }

    /// Cooperative LEAF lowering (willow-lpn.5.3 slice 4): compile `name` (an
    /// eligible no-param async fn) as a CONSTRUCTOR (its public symbol: alloc a
    /// frame whose slot 0 is the RESULT, spawn the poll fn as a task, return the
    /// frame ptr) plus a suspending poll fn whose `return v` stores `v` at the
    /// RESULT slot. `await f()` of such a fn block-runs the scheduler and reads
    /// that slot (see emit_await).
    fn compile_cooperative_leaf(&mut self, name: &str, f: &FunctionDecl) -> Result<()> {
        let poll_symbol = format!("{name}__coop_poll");
        let mut poll_sig = self.module.make_signature();
        poll_sig.params.push(AbiParam::new(types::I64));
        poll_sig.returns.push(AbiParam::new(types::I32));
        let poll_fid = self
            .module
            .declare_function(&poll_symbol, Linkage::Local, &poll_sig)?;
        self.func_ids.insert(poll_symbol.clone(), poll_fid);

        // Frame layout: slot 0 = RESULT (return type), slot 1 = TASK_ID (the
        // scheduler task id, i64 non-GC, so an AWAITER can willow_sched_await it),
        // slots 2.. = params (eligible leaves have no locals). GC mask marks
        // GC-ref slots only.
        let mut slots = vec![
            AsyncFrameSlot {
                key: f.span,
                name: "__result".to_string(),
                ty: f.return_type.clone(),
            },
            AsyncFrameSlot {
                key: crate::diagnostics::Span::new(usize::MAX, usize::MAX, 0, 0),
                name: "__task_id".to_string(),
                ty: Type::I64,
            },
        ];
        for p in &f.params {
            slots.push(AsyncFrameSlot {
                key: p.span,
                name: p.name.clone(),
                ty: p.ty.clone(),
            });
        }
        // Locals after the params (slice 4c): frame-backed so they survive the
        // leaf's own suspensions, keyed by declaration span.
        let n_params = f.params.len();
        let mut seen: HashSet<crate::diagnostics::Span> = HashSet::new();
        self.coop_collect_let_slots(&f.body, &mut slots, &mut seen);
        let layout = AsyncFrameLayout::new(slots, &self.enum_infos);
        let slot_count = layout.slot_count() as i64;
        let mask = layout.gc_slot_mask as i64;
        let result_offset = async_frame_slot_offset(0);
        let task_id_offset = async_frame_slot_offset(1);
        let param_bindings: Vec<(String, i32, Type)> = f
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| (p.name.clone(), async_frame_slot_offset(2 + i), p.ty.clone()))
            .collect();
        // Offsets for the poll fn's locals: layout slots from (2 + n_params) on.
        let mut offsets: HashMap<crate::diagnostics::Span, i32> = HashMap::new();
        for (i, slot) in layout.slots.iter().enumerate().skip(2 + n_params) {
            offsets.insert(slot.key, async_frame_slot_offset(i));
        }

        // Constructor = the fn's public symbol: alloc frame, store args into the
        // param slots, spawn the poll task, return the frame ptr (the awaitable
        // "future").
        let ctor_fid = self.func_ids[name];
        let mut ctx = self.module.make_context();
        let mut sig = self.module.make_signature();
        for p in &f.params {
            sig.params.push(AbiParam::new(clif_type(&p.ty)));
        }
        sig.returns.push(AbiParam::new(types::I64));
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, ctor_fid.as_u32());
        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let alloc_fid = self.func_ids["willow_async_frame_alloc"];
        let alloc_ref = self.module.declare_func_in_func(alloc_fid, builder.func);
        let sc = builder.ins().iconst(types::I64, slot_count);
        let mk = builder.ins().iconst(types::I64, mask);
        let call = builder.ins().call(alloc_ref, &[sc, mk]);
        let frame = builder.inst_results(call)[0];
        // Store args into their param slots (slots 2..) before spawning (no
        // allocation happens between alloc and spawn, so the unrooted frame is
        // safe).
        for (i, _p) in f.params.iter().enumerate() {
            let arg = builder.block_params(entry)[i];
            let off = async_frame_slot_offset(2 + i);
            builder.ins().store(MemFlags::trusted(), arg, frame, off);
        }
        let poll_ref = self.module.declare_func_in_func(poll_fid, builder.func);
        let poll_addr = builder.ins().func_addr(types::I64, poll_ref);
        let spawn_fid = self.func_ids["willow_sched_spawn"];
        let spawn_ref = self.module.declare_func_in_func(spawn_fid, builder.func);
        // Record the scheduler task id in slot 1 (TASK_ID) so an awaiter can
        // willow_sched_await it.
        let spawn_call = builder.ins().call(spawn_ref, &[poll_addr, frame]);
        let task_id = builder.inst_results(spawn_call)[0];
        builder
            .ins()
            .store(MemFlags::trusted(), task_id, frame, task_id_offset);
        builder.ins().return_(&[frame]);
        builder.finalize();
        self.module.define_function(ctor_fid, &mut ctx)?;
        self.module.clear_context(&mut ctx);

        // Poll fn = the state machine; params are bound from their frame slots
        // and locals are frame-backed via `offsets`.
        self.compile_coop_main_poll(
            &poll_symbol,
            f,
            offsets,
            Some(result_offset),
            &param_bindings,
        )?;
        Ok(())
    }

    /// Frame layout for a cooperative async fn body: one slot per GC-managed
    /// local (frame-backed so it survives suspension), keyed by declaration span.
    fn coop_frame_layout(&self, body: &Block) -> AsyncFrameLayout {
        let mut slots: Vec<AsyncFrameSlot> = Vec::new();
        let mut seen: HashSet<crate::diagnostics::Span> = HashSet::new();
        self.coop_collect_let_slots(body, &mut slots, &mut seen);
        AsyncFrameLayout::new(slots, &self.enum_infos)
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
        if let Some((_, await_span)) = await_coop_call(expr, &self.cooperative_leaves) {
            if seen.insert(await_span) {
                out.push(AsyncFrameSlot {
                    key: await_span,
                    name: "__callee_frame".to_string(),
                    ty: Type::Named("__coop_callee_frame".to_string()),
                });
            }
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
                Stmt::Let(l) => {
                    let ty =
                        l.ty.clone()
                            .or_else(|| self.async_local_types.get(&l.span).cloned());
                    if let Some(ty) = ty {
                        if seen.insert(l.span) {
                            out.push(AsyncFrameSlot {
                                key: l.span,
                                name: l.name.clone(),
                                ty,
                            });
                        }
                    }
                    self.coop_collect_callee_frame_slot(&l.init, out, seen);
                }
                Stmt::Assign(s) => {
                    self.coop_collect_callee_frame_slot(&s.value, out, seen);
                }
                Stmt::FieldAssign(s) => {
                    self.coop_collect_callee_frame_slot(&s.value, out, seen);
                }
                Stmt::IndexAssign(s) => {
                    self.coop_collect_callee_frame_slot(&s.value, out, seen);
                }
                Stmt::Expr(es) => {
                    self.coop_collect_callee_frame_slot(&es.expr, out, seen);
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

    /// `willow_user_main` driver: alloc frame, spawn the poll task, run the
    /// scheduler to completion.
    fn compile_coop_main_driver(
        &mut self,
        name: &str,
        poll_symbol: &str,
        slot_count: i64,
        mask: i64,
    ) -> Result<()> {
        let func_id = self.func_ids[name];
        let sig = self.module.make_signature(); // void, no params
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());
        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        // frame = willow_async_frame_alloc(slot_count, mask)
        let alloc_fid = self.func_ids["willow_async_frame_alloc"];
        let alloc_ref = self.module.declare_func_in_func(alloc_fid, builder.func);
        let slot_count_v = builder.ins().iconst(types::I64, slot_count);
        let mask_v = builder.ins().iconst(types::I64, mask);
        let call = builder.ins().call(alloc_ref, &[slot_count_v, mask_v]);
        let frame = builder.inst_results(call)[0];

        // willow_sched_spawn(poll_addr, frame)
        let poll_fid = self.func_ids[poll_symbol];
        let poll_ref = self.module.declare_func_in_func(poll_fid, builder.func);
        let poll_addr = builder.ins().func_addr(types::I64, poll_ref);
        let spawn_fid = self.func_ids["willow_sched_spawn"];
        let spawn_ref = self.module.declare_func_in_func(spawn_fid, builder.func);
        builder.ins().call(spawn_ref, &[poll_addr, frame]);

        // willow_sched_run()
        let run_fid = self.func_ids["willow_sched_run"];
        let run_ref = self.module.declare_func_in_func(run_fid, builder.func);
        builder.ins().call(run_ref, &[]);

        builder.ins().return_(&[]);
        builder.finalize();
        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// The poll-fn state machine: split the body at `await sleep(n)` points into
    /// per-state segments; the entry dispatches on the frame state word.
    fn compile_coop_main_poll(
        &mut self,
        poll_symbol: &str,
        f: &FunctionDecl,
        offsets: HashMap<crate::diagnostics::Span, i32>,
        result_offset: Option<i32>,
        param_bindings: &[(String, i32, Type)],
    ) -> Result<()> {
        let func_id = self.func_ids[poll_symbol];
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I32));
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());
        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let frame = builder.block_params(entry)[0];
        let dispatch = builder.create_block();
        builder.ins().jump(dispatch, &[]);

        // body_start is state 0; each `await` suspend appends a resume block
        // (state k = suspends[k-1]). Because all locals/params are frame-backed,
        // resume blocks need no SSA block params — we emit structured control
        // flow (if/while) directly and seal everything at the end (slice 5).
        let body_start = builder.create_block();
        let mut suspends: Vec<cranelift_codegen::ir::Block> = Vec::new();
        {
            let mut fg = FuncGen {
                builder: &mut builder,
                module: &mut self.module,
                func_ids: &self.func_ids,
                func_return_types: &self.func_return_types,
                fn_types: &self.fn_types,
                func_param_modes: &self.func_param_modes,
                func_param_debug: &self.func_param_debug,
                known_modules: &self.known_modules,
                lambda_names: &self.lambda_names,
                spawn_tramp_names: &self.spawn_tramp_names,
                spawn_fptr_sigs: &self.spawn_fptr_sigs,
                cooperative_leaves: &self.cooperative_leaves,
                string_literals: &self.string_literals,
                class_layouts: &self.class_layouts,
                static_storage: &self.static_storage,
                enum_infos: &self.enum_infos,
                class_base: &self.class_base,
                class_type_ids: &self.class_type_ids,
                lambda_return_types: &self.lambda_return_types,
                interface_infos: &self.interface_infos,
                vtable_ids: &self.vtable_ids,
                async_local_types: &self.async_local_types,
                // The frame is the poll fn's parameter (allocated + GC-rooted by
                // the driver via willow_sched_spawn); locals are frame-backed via
                // these offsets so they survive suspension.
                async_frame: Some(frame),
                async_frame_offsets: offsets,
                main_result_err_ty: None,
                vars: HashMap::new(),
                return_type: Type::Void,
                current_class: None,
                is_async: false,
                terminated: false,
                gc_root_count: 0,
                build_mode: self.build_mode,
                source_file: &self.source_file,
            };
            // Bind params from their frame slots (cooperative leaf, slice 4b):
            // the constructor stored the args there before spawning.
            for (name, offset, ty) in param_bindings {
                fg.vars.insert(
                    name.clone(),
                    VarStorage::Frame {
                        offset: *offset,
                        ty: ty.clone(),
                    },
                );
            }
            fg.builder.switch_to_block(body_start);
            let falls_through =
                fg.emit_coop_stmts(&f.body.stmts, &mut suspends, frame, result_offset);
            // Fell off the end of the body → the task is Ready.
            if falls_through {
                let ready = fg.builder.ins().iconst(types::I32, 1);
                fg.builder.ins().return_(&[ready]);
            }
        }

        // Dispatch on the state word (offset 0): state 0 → body_start,
        // state k → suspends[k-1].
        builder.switch_to_block(dispatch);
        let state = builder.ins().load(types::I64, MemFlags::new(), frame, 0i32);
        for (k, resume) in suspends.iter().enumerate() {
            let want = builder.ins().iconst(types::I64, (k + 1) as i64);
            let is_k = builder.ins().icmp(IntCC::Equal, state, want);
            let next = builder.create_block();
            builder.ins().brif(is_k, *resume, &[], next, &[]);
            builder.switch_to_block(next);
        }
        builder.ins().jump(body_start, &[]);
        builder.seal_all_blocks();

        builder.finalize();
        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    fn compile_function_named(&mut self, name: &str, f: &FunctionDecl) -> Result<()> {
        let func_id = self.func_ids[name];
        // `willow_user_main` is always parameterless (the runtime calls it with
        // no arguments). A declared `fn main(args: Array<String>)` parameter is
        // bound from the runtime inside the body instead of via a call argument.
        // `name` here is the lookup name (`main`), so map it to the symbol.
        let is_main = user_function_symbol(name) == USER_MAIN_SYMBOL;

        // Cooperative async lowering for an eligible `async fn main` (willow-lpn.5.3
        // Stage 2). Strictly gated, so all other functions keep the eager path.
        if is_main
            && cooperative_main_eligible(
                f,
                &self.async_local_types,
                &self.enum_infos,
                &self.cooperative_leaves,
            )
        {
            return self.compile_cooperative_main(name, f);
        }
        if !is_main && self.cooperative_leaves.contains(name) {
            return self.compile_cooperative_leaf(name, f);
        }

        let mut sig = self.module.make_signature();
        let ptr_ty = self.module.target_config().pointer_type();
        if !is_main {
            for param in &f.params {
                sig.params
                    .push(AbiParam::new(param_abi_type(param, ptr_ty)));
            }
        }
        let call_return_type = function_call_return_type(f);
        // For a `Result<void, E>` main, the error payload type `E` drives the
        // exit/report path emitted at each return.
        let main_result_err_ty: Option<Type> = if is_main {
            main_result_err_type(f)
        } else {
            None
        };
        // A `Result<void, E>` main lowers to a VOID `willow_user_main` — it
        // inspects its result inside the body and exits accordingly (willow-exg),
        // so the runtime keeps calling `willow_user_main()` uniformly. Other
        // mains (incl. async, whose body returns a Future) keep their signature.
        let force_void_main = main_result_err_ty.is_some();
        if call_return_type != Type::Void && !force_void_main {
            sig.returns
                .push(AbiParam::new(clif_type(&call_return_type)));
        }

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());

        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);

        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        let mut fg = FuncGen {
            builder: &mut builder,
            module: &mut self.module,
            func_ids: &self.func_ids,
            func_return_types: &self.func_return_types,
            fn_types: &self.fn_types,
            func_param_modes: &self.func_param_modes,
            func_param_debug: &self.func_param_debug,
            known_modules: &self.known_modules,
            lambda_names: &self.lambda_names,
            spawn_tramp_names: &self.spawn_tramp_names,
            spawn_fptr_sigs: &self.spawn_fptr_sigs,
            cooperative_leaves: &self.cooperative_leaves,
            string_literals: &self.string_literals,
            class_layouts: &self.class_layouts,
            static_storage: &self.static_storage,
            enum_infos: &self.enum_infos,
            class_base: &self.class_base,
            class_type_ids: &self.class_type_ids,
            lambda_return_types: &self.lambda_return_types,
            interface_infos: &self.interface_infos,
            vtable_ids: &self.vtable_ids,
            async_local_types: &self.async_local_types,
            async_frame: None,
            async_frame_offsets: HashMap::new(),
            main_result_err_ty,
            vars: HashMap::new(),
            return_type: f.return_type.clone(),
            current_class: None,
            is_async: f.is_async,
            terminated: false,
            gc_root_count: 0,
            build_mode: self.build_mode,
            source_file: &self.source_file,
        };

        // Async fns (except `main`, which has special arg binding) allocate a
        // heap frame and store GC-managed params/locals into it so they survive
        // `await` (willow-lpn.5a/5b). Eager execution is unchanged. After this,
        // `fg.async_frame_offsets` maps each frame-backed name to its offset.
        if f.is_async && !is_main {
            fg.setup_async_frame(&f.params, &f.body);
        }

        // Bind params
        if is_main {
            // Bind a declared `args: Array<String>` parameter from the process
            // arguments. `willow_user_main` itself takes no parameters.
            if let Some(param) = f.params.first() {
                let arr_id = fg.func_ids["willow_runtime_args_array"];
                let arr_ref = fg.module.declare_func_in_func(arr_id, fg.builder.func);
                let call = fg.builder.ins().call(arr_ref, &[]);
                let arr = fg.builder.inst_results(call)[0];
                fg.bind_param(&param.name, &param.ty, &param.mode, arr);
            }
        } else {
            for (i, param) in f.params.iter().enumerate() {
                let val = fg.builder.block_params(entry_block)[i];
                // Frame-back a GC-managed value param (its name is in the map).
                let framed = matches!(param.mode, ParamMode::Value)
                    .then(|| fg.async_frame_offsets.get(&param.span).copied())
                    .flatten();
                if let Some(offset) = framed {
                    fg.bind_param_framed(&param.name, &param.ty, val, offset);
                    continue;
                }
                fg.bind_param(&param.name, &param.ty, &param.mode, val);
            }
        }

        fg.emit_block(&f.body);

        // Implicit return at end of function body.
        if !fg.terminated {
            // Pop any GC roots that were pushed for parameters.
            if fg.gc_root_count > 0 {
                fg.emit_pop_roots_n(fg.gc_root_count);
            }
            if fg.is_async {
                let future = fg.emit_ready_future_void();
                fg.builder.ins().return_(&[future]);
            } else {
                fg.builder.ins().return_(&[]);
            }
        }

        builder.finalize();
        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
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

    fn declare_class_methods(&mut self, c: &ClassDecl) -> Result<()> {
        for m in &c.methods {
            let mangled = self.class_method_symbol(&c.name, &m.name);
            let mut sig = self.module.make_signature();
            let ptr_ty = self.module.target_config().pointer_type();
            sig.params.push(AbiParam::new(types::I64)); // self pointer
            for p in &m.params {
                sig.params.push(AbiParam::new(param_abi_type(p, ptr_ty)));
            }
            let call_return_type = method_call_return_type(m);
            if call_return_type != Type::Void {
                sig.returns
                    .push(AbiParam::new(clif_type(&call_return_type)));
            }
            let id = self
                .module
                .declare_function(&mangled, Linkage::Local, &sig)?;
            self.func_ids.insert(mangled.clone(), id);
            self.func_return_types
                .insert(mangled.clone(), call_return_type.clone());
            self.func_param_modes.insert(
                mangled.clone(),
                m.params.iter().map(|p| p.mode.clone()).collect(),
            );
            self.func_param_debug
                .insert(mangled.clone(), param_debug_from_params(&m.params));
            let mut param_types = vec![Type::Named(c.name.clone())]; // self
            param_types.extend(m.params.iter().map(|p| p.ty.clone()));
            self.fn_types
                .insert(mangled, Type::Fn(param_types, Box::new(call_return_type)));
        }
        Ok(())
    }

    /// Declare global storage for each `static [mut] name: T = expr` property and
    /// record its initializer for `__willow_static_init` (willow-qsqf §13.3/§11).
    /// `class_key` is the registered (possibly module-qualified) class name.
    fn declare_static_storage_for_class(&mut self, class_key: &str, c: &ClassDecl) -> Result<()> {
        for field in &c.fields {
            if !field.is_static {
                continue;
            }
            let Some(init) = &field.initializer else {
                continue;
            };
            let key = (class_key.to_string(), field.name.clone());
            if self.static_storage.contains_key(&key) {
                continue;
            }
            let sym = format!("{}__static__{}", class_key.replace("::", "__"), field.name);
            let data_id = self
                .module
                .declare_data(&sym, Linkage::Local, true, false)?;
            let mut data = DataDescription::new();
            // Zero-initialized: GC-managed slots start null so a collection during
            // static init sees a safe (null) slot (willow-qsqf §12.3).
            data.define_zeroinit(8);
            self.module.define_data(data_id, &data)?;
            self.static_storage.insert(
                key,
                StaticStorageInfo {
                    data_id,
                    ty: field.ty.clone(),
                },
            );
            self.static_init_order.push(StaticInitItem {
                class_key: class_key.to_string(),
                field: field.name.clone(),
                init: init.clone(),
                ty: field.ty.clone(),
            });
        }
        Ok(())
    }

    /// Compile `__willow_static_init`: evaluate every static-property initializer
    /// in declaration order, store it into global storage, and register
    /// GC-managed slots as permanent roots (willow-qsqf §11/§12). Called once at
    /// the start of `willow_user_main`.
    fn compile_static_init(&mut self) -> Result<()> {
        let items = self.static_init_order.clone();
        let func_id = self.func_ids[STATIC_INIT_SYMBOL];

        let mut sig = self.module.make_signature();
        let _ = &mut sig; // no params, no returns
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());

        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let mut fg = FuncGen {
            builder: &mut builder,
            module: &mut self.module,
            func_ids: &self.func_ids,
            func_return_types: &self.func_return_types,
            fn_types: &self.fn_types,
            func_param_modes: &self.func_param_modes,
            func_param_debug: &self.func_param_debug,
            known_modules: &self.known_modules,
            lambda_names: &self.lambda_names,
            spawn_tramp_names: &self.spawn_tramp_names,
            spawn_fptr_sigs: &self.spawn_fptr_sigs,
            cooperative_leaves: &self.cooperative_leaves,
            string_literals: &self.string_literals,
            class_layouts: &self.class_layouts,
            static_storage: &self.static_storage,
            enum_infos: &self.enum_infos,
            class_base: &self.class_base,
            class_type_ids: &self.class_type_ids,
            lambda_return_types: &self.lambda_return_types,
            interface_infos: &self.interface_infos,
            vtable_ids: &self.vtable_ids,
            async_local_types: &self.async_local_types,
            async_frame: None,
            async_frame_offsets: HashMap::new(),
            main_result_err_ty: None,
            vars: HashMap::new(),
            return_type: Type::Void,
            current_class: None,
            is_async: false,
            terminated: false,
            gc_root_count: 0,
            build_mode: self.build_mode,
            source_file: &self.source_file,
        };

        let ptr_ty = fg.module.target_config().pointer_type();
        for item in &items {
            // Initializers reference other statics by explicit class name
            // (`C::a`); `Self::` is not resolved here in the MVP.
            let val = fg.emit_expr_coerced(&item.init, &item.ty);
            let info = &fg.static_storage[&(item.class_key.clone(), item.field.clone())];
            let gv = fg
                .module
                .declare_data_in_func(info.data_id, fg.builder.func);
            let addr = fg.builder.ins().global_value(ptr_ty, gv);
            fg.builder.ins().store(MemFlags::new(), val, addr, 0);
            // GC-managed statics: root the slot permanently so the collector
            // traces the current value (also correct for `static mut`).
            if is_gc_managed(&item.ty, fg.enum_infos) {
                let push_id = fg.func_ids["willow_push_root"];
                let push_ref = fg.module.declare_func_in_func(push_id, fg.builder.func);
                fg.builder.ins().call(push_ref, &[addr]);
            }
        }
        fg.builder.ins().return_(&[]);
        builder.finalize();
        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// Emit a static vtable per `(class, implemented-interface)` pair. Each
    /// vtable is `slot_count` function pointers in the interface's declaration
    /// (method) order; slot K points at the concrete method the class provides
    /// (found in the class itself or an ancestor). See spec §8.2 / §9.5.
    fn declare_interface_vtables(&mut self, program: &Program) -> Result<()> {
        let classes: Vec<ClassDecl> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        self.declare_vtables_for_classes(&classes)
    }

    /// Emit `(class, interface)` vtables for the given (already module-qualified)
    /// class declarations. Used for both the entry program and imported modules.
    fn declare_vtables_for_classes(&mut self, classes: &[ClassDecl]) -> Result<()> {
        for c in classes {
            for iface_ty in &c.implements {
                // The vtable layout (method slots) is keyed by the interface name;
                // generic type arguments do not change the class's method func ids.
                // A class may implement several instantiations of one generic
                // interface (`Container<i64>`, `Container<String>`): every slot
                // points to a monomorphic class method (default-method bodies are
                // injected once as class methods too), so all instantiations yield
                // a byte-identical vtable and correctly share this single name-keyed
                // entry — `declare_one_vtable` dedups them (willow-1js.6).
                let iface_name = match iface_ty {
                    Type::Named(n) | Type::Generic(n, _) => n.clone(),
                    _ => continue,
                };
                let Some(iface) = self.interface_infos.get(&iface_name).cloned() else {
                    continue; // unknown interface already reported by the type checker
                };
                self.declare_one_vtable(&c.name, &iface)?;
            }
        }
        Ok(())
    }

    fn declare_one_vtable(&mut self, class_name: &str, iface: &InterfaceInfo) -> Result<()> {
        let key = (class_name.to_string(), iface.name.clone());
        if self.vtable_ids.contains_key(&key) {
            return Ok(());
        }
        let slot_count = iface.method_order.len().max(1);
        let symbol = format!(
            "{}__as__{}__vtable",
            backend_symbol_component(class_name),
            backend_symbol_component(&iface.name)
        );
        let data_id = self
            .module
            .declare_data(&symbol, Linkage::Local, false, false)?;
        let mut data = DataDescription::new();
        // Explicit zeroed bytes (not `define_zeroinit`, which is BSS and cannot
        // carry the function-address relocations written below).
        data.define(vec![0u8; slot_count * 8].into_boxed_slice());
        for (slot, method_name) in iface.method_order.iter().enumerate() {
            if let Some(func_id) = self.resolve_class_method_func_id(class_name, method_name) {
                let func_ref = self.module.declare_func_in_data(func_id, &mut data);
                data.write_function_addr((slot * 8) as u32, func_ref);
            }
        }
        self.module.define_data(data_id, &data)?;
        self.vtable_ids.insert(key, data_id);
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

    fn compile_class_methods(&mut self, c: &ClassDecl) -> Result<()> {
        for m in &c.methods {
            self.compile_class_method(c, m)?;
        }
        Ok(())
    }

    fn compile_class_method(&mut self, c: &ClassDecl, m: &MethodDecl) -> Result<()> {
        let mangled = self.class_method_symbol(&c.name, &m.name);
        let func_id = self.func_ids[&mangled];

        let mut sig = self.module.make_signature();
        let ptr_ty = self.module.target_config().pointer_type();
        sig.params.push(AbiParam::new(types::I64)); // self pointer
        for p in &m.params {
            sig.params.push(AbiParam::new(param_abi_type(p, ptr_ty)));
        }
        let call_return_type = method_call_return_type(m);
        if call_return_type != Type::Void {
            sig.returns
                .push(AbiParam::new(clif_type(&call_return_type)));
        }

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());

        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);

        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        let mut fg = FuncGen {
            builder: &mut builder,
            module: &mut self.module,
            func_ids: &self.func_ids,
            func_return_types: &self.func_return_types,
            fn_types: &self.fn_types,
            func_param_modes: &self.func_param_modes,
            func_param_debug: &self.func_param_debug,
            known_modules: &self.known_modules,
            lambda_names: &self.lambda_names,
            spawn_tramp_names: &self.spawn_tramp_names,
            spawn_fptr_sigs: &self.spawn_fptr_sigs,
            cooperative_leaves: &self.cooperative_leaves,
            string_literals: &self.string_literals,
            class_layouts: &self.class_layouts,
            static_storage: &self.static_storage,
            enum_infos: &self.enum_infos,
            class_base: &self.class_base,
            class_type_ids: &self.class_type_ids,
            lambda_return_types: &self.lambda_return_types,
            interface_infos: &self.interface_infos,
            vtable_ids: &self.vtable_ids,
            async_local_types: &self.async_local_types,
            async_frame: None,
            async_frame_offsets: HashMap::new(),
            main_result_err_ty: None,
            vars: HashMap::new(),
            return_type: m.return_type.clone(),
            current_class: Some(c.name.as_str()),
            is_async: m.is_async,
            terminated: false,
            gc_root_count: 0,
            build_mode: self.build_mode,
            source_file: &self.source_file,
        };

        // Bind `self` as the first parameter for INSTANCE methods only.
        // The uniform method ABI keeps a hidden first param slot even for static
        // methods (static `::` calls pass a dummy null there), so user params
        // always start at block_params[1]. A static method simply does not bind
        // `self`: there is no receiver, and the body cannot reference it
        // (rejected by the type checker, willow-qsqf §9.2).
        //
        // The receiver is a GC-managed class object; it must be stored in a
        // stack slot and rooted so that allocations inside the method body
        // cannot cause the receiver to be collected.
        if !m.is_static {
            let self_val = fg.builder.block_params(entry_block)[0];
            let self_slot = fg.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                0,
            ));
            fg.builder.ins().stack_store(self_val, self_slot, 0);
            {
                let ptr_ty = fg.module.target_config().pointer_type();
                let addr = fg.builder.ins().stack_addr(ptr_ty, self_slot, 0);
                let push_id = fg.func_ids["willow_push_root"];
                let push_ref = fg.module.declare_func_in_func(push_id, fg.builder.func);
                fg.builder.ins().call(push_ref, &[addr]);
                fg.gc_root_count += 1;
            }
            let receiver_ty = Type::Named(c.name.clone());
            let receiver_storage = VarStorage::Stack {
                slot: self_slot,
                ty: receiver_ty,
            };
            fg.vars.insert("self".to_string(), receiver_storage);
        }

        // Bind remaining method params
        for (i, p) in m.params.iter().enumerate() {
            let val = fg.builder.block_params(entry_block)[i + 1];
            fg.bind_param(&p.name, &p.ty, &p.mode, val);
        }

        fg.emit_block(&m.body);

        if !fg.terminated {
            // Pop any GC roots (self, params) before the implicit void return.
            if fg.gc_root_count > 0 {
                fg.emit_pop_roots_n(fg.gc_root_count);
            }
            if fg.is_async {
                let future = fg.emit_ready_future_void();
                fg.builder.ins().return_(&[future]);
            } else {
                fg.builder.ins().return_(&[]);
            }
        }

        builder.finalize();
        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
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
    func_ids: &'a HashMap<String, FuncId>,
    func_return_types: &'a HashMap<String, Type>,
    fn_types: &'a HashMap<String, Type>,
    func_param_modes: &'a HashMap<String, Vec<ParamMode>>,
    func_param_debug: &'a HashMap<String, Vec<ParamDebug>>,
    known_modules: &'a HashMap<String, String>,
    lambda_names: &'a HashMap<crate::diagnostics::Span, String>,
    spawn_tramp_names: &'a HashMap<crate::diagnostics::Span, String>,
    spawn_fptr_sigs: &'a HashMap<crate::diagnostics::Span, (Vec<Type>, Type)>,
    cooperative_leaves: &'a std::collections::HashSet<String>,
    string_literals: &'a HashMap<String, DataId>,
    class_layouts: &'a HashMap<String, Vec<(String, Type)>>,
    static_storage: &'a HashMap<(String, String), StaticStorageInfo>,
    enum_infos: &'a HashMap<String, EnumInfo>,
    class_base: &'a HashMap<String, String>,
    /// Maps class name → unique type_id (i64) stored at word 0 of every class object.
    class_type_ids: &'a HashMap<String, i64>,
    /// Type-checker-inferred return types for lambdas without explicit annotations.
    lambda_return_types: &'a HashMap<crate::diagnostics::Span, Type>,
    /// Interface metadata for method dispatch + boxing.
    interface_infos: &'a HashMap<String, InterfaceInfo>,
    /// Static `(class, interface)` vtable data objects for class→interface boxing.
    vtable_ids: &'a HashMap<(String, String), DataId>,
    /// Resolved types of async-fn locals (keyed by span) for frame-backing
    /// unannotated live-across-await locals (willow-lpn.5c).
    async_local_types: &'a HashMap<crate::diagnostics::Span, Type>,
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
fn async_frame_slot_offset(n: usize) -> i32 {
    ASYNC_FRAME_HEADER_BYTES + (n as i32) * 8
}

impl<'a, 'b> FuncGen<'a, 'b> {
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
                let push_id = self.func_ids["willow_push_root"];
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
        self.builder.ins().store(MemFlags::new(), val, base, offset);
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

    fn setup_async_frame(&mut self, params: &[Param], body: &Block) -> Option<AsyncFrameLayout> {
        let slots = self.collect_async_frame_slots_resolved(params, body);
        let layout = AsyncFrameLayout::new(slots, self.enum_infos);

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
            return None;
        }

        let slot_count = self
            .builder
            .ins()
            .iconst(types::I64, layout.slot_count() as i64);
        let mask = self
            .builder
            .ins()
            .iconst(types::I64, layout.gc_slot_mask as i64);
        let alloc_id = self.func_ids["willow_async_frame_alloc"];
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
        Some(layout)
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
                    .load(clif_type(ty), MemFlags::new(), ptr, 0)
            }
            VarStorage::Frame { offset, ty } => {
                let base = self
                    .async_frame
                    .expect("frame-backed var requires an allocated async frame");
                self.builder
                    .ins()
                    .load(clif_type(ty), MemFlags::new(), base, *offset)
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
                    .store(MemFlags::new(), val, base, *offset);
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
        self.builder.ins().store(MemFlags::new(), val, ptr, 0);
    }

    fn emit_reference_write_barrier_hook(
        &mut self,
        _ptr: cranelift_codegen::ir::Value,
        _val: cranelift_codegen::ir::Value,
        ty: &Type,
    ) {
        if is_gc_managed(ty, self.enum_infos) {
            // Current stop-the-world GC does not require a write barrier. Keep all
            // indirect reference stores flowing through this hook so a future
            // generational/concurrent collector can attach one in a single place.
        }
    }

    fn address_of_var(&mut self, storage: &VarStorage) -> cranelift_codegen::ir::Value {
        match storage {
            VarStorage::Stack { slot, .. } => {
                let ptr_ty = self.module.target_config().pointer_type();
                self.builder.ins().stack_addr(ptr_ty, *slot, 0)
            }
            VarStorage::ReferencePtr { var, .. } => self.builder.use_var(*var),
            VarStorage::Frame { offset, .. } => {
                // Address of the frame slot (frame is GC-rooted and non-moving).
                let base = self
                    .async_frame
                    .expect("frame-backed var requires an allocated async frame");
                self.builder.ins().iadd_imm(base, *offset as i64)
            }
            VarStorage::Value { var, .. } => {
                let ptr_ty = self.module.target_config().pointer_type();
                let val = self.builder.use_var(*var);
                let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    0,
                ));
                self.builder.ins().stack_store(val, slot, 0);
                self.builder.ins().stack_addr(ptr_ty, slot, 0)
            }
        }
    }

    /// Push a GC root for a pointer value. Creates a stack slot to hold the pointer so
    /// the GC can find and mark the object via `willow_push_root`.
    fn emit_push_root(&mut self, val: cranelift_codegen::ir::Value) {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        self.builder.ins().stack_store(val, slot, 0);
        let ptr_ty = self.module.target_config().pointer_type();
        let addr = self.builder.ins().stack_addr(ptr_ty, slot, 0);
        let push_id = self.func_ids["willow_push_root"];
        let push_ref = self.module.declare_func_in_func(push_id, self.builder.func);
        self.builder.ins().call(push_ref, &[addr]);
        self.gc_root_count += 1;
    }

    fn emit_push_root_slot(&mut self, slot: cranelift_codegen::ir::StackSlot) {
        let ptr_ty = self.module.target_config().pointer_type();
        let addr = self.builder.ins().stack_addr(ptr_ty, slot, 0);
        let push_id = self.func_ids["willow_push_root"];
        let push_ref = self.module.declare_func_in_func(push_id, self.builder.func);
        self.builder.ins().call(push_ref, &[addr]);
        self.gc_root_count += 1;
    }

    /// Pop `n` GC roots by calling `willow_pop_roots(n)`.
    fn emit_pop_roots_n(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let pop_id = self.func_ids["willow_pop_roots"];
        let pop_ref = self.module.declare_func_in_func(pop_id, self.builder.func);
        let n_val = self.builder.ins().iconst(types::I32, n as i64);
        self.builder.ins().call(pop_ref, &[n_val]);
    }

    fn emit_ready_future_void(&mut self) -> cranelift_codegen::ir::Value {
        let fid = self.func_ids["willow_future_ready_void"];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[]);
        self.builder.inst_results(call)[0]
    }

    /// Box a concrete class instance into an interface value: a 16-byte GC object
    /// `[object (GC ref) | vtable (raw)]` allocated with `gc_ref_mask = 0b01`.
    /// Returns the box pointer (spec §8.1 / §9.2).
    fn emit_interface_box(
        &mut self,
        object: cranelift_codegen::ir::Value,
        class_name: &str,
        interface_name: &str,
    ) -> cranelift_codegen::ir::Value {
        let key = (class_name.to_string(), interface_name.to_string());
        let Some(&vtable_id) = self.vtable_ids.get(&key) else {
            // No vtable registered (e.g. unknown interface already diagnosed):
            // fall back to the raw object so codegen stays total.
            return object;
        };

        // Root the object across the box allocation (the alloc may collect).
        self.emit_push_root(object);
        let size = self.builder.ins().iconst(types::I64, 16);
        let mask = self.builder.ins().iconst(types::I64, 0b01);
        let alloc_id = self.func_ids["willow_alloc_typed"];
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let box_ptr = self.builder.inst_results(call)[0];

        // word 0: concrete object pointer (GC-traced). The GC is non-moving, so
        // the rooted `object` value is still valid after any collection above.
        self.builder
            .ins()
            .store(MemFlags::new(), object, box_ptr, 0i32);

        // word 1: vtable address (a static data symbol; not a GC reference).
        let gv = self
            .module
            .declare_data_in_func(vtable_id, self.builder.func);
        let ptr_ty = self.module.target_config().pointer_type();
        let vtable_ptr = self.builder.ins().global_value(ptr_ty, gv);
        self.builder
            .ins()
            .store(MemFlags::new(), vtable_ptr, box_ptr, 8i32);

        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        box_ptr
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

    /// Emit `expr`, then coerce the result to `target_ty` (class→interface box).
    fn emit_expr_coerced(&mut self, expr: &Expr, target_ty: &Type) -> cranelift_codegen::ir::Value {
        // An array literal with a known target element type emits its elements
        // boxed to that type (so `let a: Array<Animal> = [Dog {}]` stores boxes,
        // and `[]` becomes a reference-element array).
        if let Expr::ArrayLiteral(elements, _) = expr {
            let target_inner = match target_ty {
                Type::Nullable(inner) => inner.as_ref(),
                other => other,
            };
            if let Type::Array(elem) = target_inner {
                let elem = (**elem).clone();
                return self.emit_array_literal(elements, &elem);
            }
        }
        let value = self.emit_expr(expr);
        let value_ty = self.ast_type_of(expr);
        self.coerce_to_target(value, &value_ty, target_ty)
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

    fn emit_ready_future(
        &mut self,
        ty: &Type,
        value: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let runtime_name = future_ready_runtime_name(ty);
        let fid = self.func_ids[runtime_name];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[value]);
        self.builder.inst_results(call)[0]
    }

    fn emit_await(&mut self, await_expr: &AwaitExpr) -> cranelift_codegen::ir::Value {
        let future_ty = self.ast_type_of(&await_expr.expr);
        let output_ty = future_output_type(&future_ty).unwrap_or(Type::Void);

        // `await f()` where f is a cooperative LEAF (willow-lpn.5.3 slice 4):
        // calling f spawned it as a task and returned its frame; block-run the
        // scheduler (drives f + its timer sleeps to completion), then read the
        // RESULT slot. The awaiter is eager, so there is no re-entrant poll.
        if let Expr::Call(c) = &await_expr.expr {
            if self.cooperative_leaves.contains(&c.callee) {
                let frame = self.emit_expr(&await_expr.expr);
                self.emit_push_root(frame);
                let run_fid = self.func_ids["willow_sched_run"];
                let run_ref = self.module.declare_func_in_func(run_fid, self.builder.func);
                self.builder.ins().call(run_ref, &[]);
                let result_off = async_frame_slot_offset(0);
                let result = self.builder.ins().load(
                    clif_type(&output_ty),
                    MemFlags::new(),
                    frame,
                    result_off,
                );
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
                return result;
            }
        }

        let future = self.emit_expr(&await_expr.expr);
        let runtime_name = future_await_runtime_name(&output_ty);
        let fid = self.func_ids[runtime_name];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[future]);
        self.builder.inst_results(call)[0]
    }

    /// Emit a call-await (`await <coop-leaf-call>`) as a suspend point
    /// (willow-lpn.5.3.1): call the callee constructor (which schedules the
    /// callee task and returns its frame), stash the frame in the awaiter's
    /// callee-frame slot, then `willow_sched_await(callee task id)`. If the
    /// callee already completed (returns 1) resume inline; otherwise store the
    /// resume state, return Pending, and resume when the scheduler wakes us. On
    /// resume, optionally read the callee's RESULT slot for let/assign/return.
    fn emit_coop_call_await(
        &mut self,
        call: &CallExpr,
        await_span: crate::diagnostics::Span,
        bind: Option<(String, i32, Type)>,
        result_ty: Option<Type>,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
    ) -> Option<cranelift_codegen::ir::Value> {
        // 1. callee = ctor(args): schedules the callee task, returns its frame.
        let modes = self.func_param_modes.get(&call.callee).cloned();
        let (arg_vals, arg_roots) =
            self.emit_call_args_rooted(Some(&call.callee), modes.as_deref(), None, &call.args);
        let ctor_fid = self.func_ids[&call.callee];
        let ctor_ref = self
            .module
            .declare_func_in_func(ctor_fid, self.builder.func);
        let c = self.builder.ins().call(ctor_ref, &arg_vals);
        let callee_frame = self.builder.inst_results(c)[0];
        if arg_roots > 0 {
            self.emit_pop_roots_n(arg_roots);
            self.gc_root_count -= arg_roots;
        }
        // 2. keep the callee frame alive across our suspension (frame-backed slot).
        let callee_off = self.async_frame_offsets[&await_span];
        self.builder
            .ins()
            .store(MemFlags::new(), callee_frame, frame, callee_off);
        // 3. id = callee[TASK_ID] (slot 1).
        let id = self.builder.ins().load(
            types::I64,
            MemFlags::new(),
            callee_frame,
            async_frame_slot_offset(1),
        );
        // 4. done = willow_sched_await(id): 1 = already complete, 0 = registered.
        let await_fid = self.func_ids["willow_sched_await"];
        let await_ref = self
            .module
            .declare_func_in_func(await_fid, self.builder.func);
        let dcall = self.builder.ins().call(await_ref, &[id]);
        let done = self.builder.inst_results(dcall)[0];
        let resume_b = self.builder.create_block();
        let suspend_b = self.builder.create_block();
        let zero = self.builder.ins().iconst(types::I32, 0);
        let is_done = self.builder.ins().icmp(IntCC::NotEqual, done, zero);
        self.builder
            .ins()
            .brif(is_done, resume_b, &[], suspend_b, &[]);
        // suspend: record resume state (1-based index of resume_b), return Pending.
        self.builder.switch_to_block(suspend_b);
        let state = (suspends.len() + 1) as i64;
        let st = self.builder.ins().iconst(types::I64, state);
        self.builder.ins().store(MemFlags::new(), st, frame, 0i32);
        let pending = self.builder.ins().iconst(types::I32, 0);
        self.builder.ins().return_(&[pending]);
        suspends.push(resume_b);
        // resume (reached from the dispatch on wake AND the already-complete brif):
        // reload the callee frame, read its RESULT slot, bind.
        self.builder.switch_to_block(resume_b);
        let result_ty = bind.as_ref().map(|(_, _, ty)| ty.clone()).or(result_ty);
        let result = result_ty.map(|ty| {
            let callee2 = self
                .builder
                .ins()
                .load(types::I64, MemFlags::new(), frame, callee_off);
            self.builder.ins().load(
                clif_type(&ty),
                MemFlags::new(),
                callee2,
                async_frame_slot_offset(0),
            )
        });
        if let Some((name, x_off, x_ty)) = bind {
            let result = result.expect("binding a call-await requires a result value");
            self.builder
                .ins()
                .store(MemFlags::new(), result, frame, x_off);
            self.vars.insert(
                name,
                VarStorage::Frame {
                    offset: x_off,
                    ty: x_ty,
                },
            );
        }
        result
    }

    /// Emit a cooperative channel `recv` (willow-dsw) as a suspend point and
    /// return the received value. A check block (a resume target) probes
    /// `willow_channel_recv_ready`: if ready (a value is queued or the channel is
    /// closed) it reads the value via `willow_channel_recv_*`; otherwise it stored
    /// the running task as a channel waiter, so we record the resume state and
    /// return Pending. A `send`/`close` later wakes us and re-enters the check.
    fn emit_coop_recv(
        &mut self,
        ch_expr: &Expr,
        elem_ty: &Type,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let check_b = self.builder.create_block();
        self.builder.ins().jump(check_b, &[]);
        let state = (suspends.len() + 1) as i64;
        suspends.push(check_b);
        self.builder.switch_to_block(check_b);
        let ch = self.emit_expr(ch_expr);
        let ready_fid = self.func_ids["willow_channel_recv_ready"];
        let ready_ref = self
            .module
            .declare_func_in_func(ready_fid, self.builder.func);
        let rcall = self.builder.ins().call(ready_ref, &[ch]);
        let ready = self.builder.inst_results(rcall)[0];
        let get_b = self.builder.create_block();
        let suspend_b = self.builder.create_block();
        let zero = self.builder.ins().iconst(types::I32, 0);
        let is_ready = self.builder.ins().icmp(IntCC::NotEqual, ready, zero);
        self.builder
            .ins()
            .brif(is_ready, get_b, &[], suspend_b, &[]);
        // Not ready: we were registered as a channel waiter; record the resume
        // state (this check block) and return Pending.
        self.builder.switch_to_block(suspend_b);
        let st = self.builder.ins().iconst(types::I64, state);
        self.builder.ins().store(MemFlags::new(), st, frame, 0i32);
        let pending = self.builder.ins().iconst(types::I32, 0);
        self.builder.ins().return_(&[pending]);
        // Ready: read the value (present, or a default if the channel is closed).
        self.builder.switch_to_block(get_b);
        let recv_name = format!("willow_channel_recv_{}", channel_runtime_suffix(elem_ty));
        let recv_fid = self.func_ids[&recv_name];
        let recv_ref = self
            .module
            .declare_func_in_func(recv_fid, self.builder.func);
        let ch2 = self.emit_expr(ch_expr);
        let vcall = self.builder.ins().call(recv_ref, &[ch2]);
        self.builder.inst_results(vcall)[0]
    }

    /// Emit a statement sequence for a cooperative poll fn (willow-lpn.5.3 slice
    /// 5). Structured control flow (`if`/`while`) becomes Cranelift blocks; each
    /// `await sleep(n)` suspends (registers the timer, stores the resume state,
    /// returns Pending) and continues in a fresh resume block whose state is its
    /// 1-based index in `suspends`. A call-await (`await <coop-leaf-call>`) and a
    /// channel `recv()` are suspend points too. `return v` stores `v` at
    /// `result_offset` and returns Ready. Locals/params are frame-backed, so no
    /// block params are needed and all blocks are sealed together by the caller.
    fn emit_coop_stmts(
        &mut self,
        stmts: &[Stmt],
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
        result_offset: Option<i32>,
    ) -> bool {
        for stmt in stmts {
            let falls_through = match stmt {
                Stmt::Expr(es) if await_sleep_arg(&es.expr).is_some() => {
                    let arg = await_sleep_arg(&es.expr).unwrap().clone();
                    let n = self.emit_expr(&arg);
                    let sleep_fid = self.func_ids["willow_sched_sleep"];
                    let sleep_ref = self
                        .module
                        .declare_func_in_func(sleep_fid, self.builder.func);
                    self.builder.ins().call(sleep_ref, &[n]);
                    let state = (suspends.len() + 1) as i64;
                    let st = self.builder.ins().iconst(types::I64, state);
                    self.builder.ins().store(MemFlags::new(), st, frame, 0i32);
                    let pending = self.builder.ins().iconst(types::I32, 0);
                    self.builder.ins().return_(&[pending]);
                    let resume = self.builder.create_block();
                    suspends.push(resume);
                    self.builder.switch_to_block(resume);
                    true
                }
                Stmt::Expr(es) if await_coop_call(&es.expr, self.cooperative_leaves).is_some() => {
                    let (call, await_span) =
                        await_coop_call(&es.expr, self.cooperative_leaves).unwrap();
                    self.emit_coop_call_await(call, await_span, None, None, suspends, frame);
                    true
                }
                Stmt::Let(l) if await_coop_call(&l.init, self.cooperative_leaves).is_some() => {
                    let (call, await_span) =
                        await_coop_call(&l.init, self.cooperative_leaves).unwrap();
                    let x_ty =
                        l.ty.clone()
                            .or_else(|| self.async_local_types.get(&l.span).cloned())
                            .unwrap_or(Type::I64);
                    let x_off = self.async_frame_offsets[&l.span];
                    self.emit_coop_call_await(
                        call,
                        await_span,
                        Some((l.name.clone(), x_off, x_ty)),
                        None,
                        suspends,
                        frame,
                    );
                    true
                }
                Stmt::Assign(a) if await_coop_call(&a.value, self.cooperative_leaves).is_some() => {
                    let (call, await_span) =
                        await_coop_call(&a.value, self.cooperative_leaves).unwrap();
                    if let Some(storage) = self.vars.get(&a.name).cloned() {
                        let target_ty = storage.ty().clone();
                        let result = self
                            .emit_coop_call_await(
                                call,
                                await_span,
                                None,
                                Some(target_ty),
                                suspends,
                                frame,
                            )
                            .expect("assignment call-await requires a result value");
                        self.store_var(&storage, result);
                    }
                    true
                }
                Stmt::Let(l) if is_channel_recv(&l.init).is_some() => {
                    let m = is_channel_recv(&l.init).unwrap();
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(&m.object)).unwrap_or(Type::I64);
                    let v = self.emit_coop_recv(&m.object, &elem_ty, suspends, frame);
                    let x_off = self.async_frame_offsets[&l.span];
                    let x_ty =
                        l.ty.clone()
                            .or_else(|| self.async_local_types.get(&l.span).cloned())
                            .unwrap_or_else(|| elem_ty.clone());
                    self.builder.ins().store(MemFlags::new(), v, frame, x_off);
                    self.vars.insert(
                        l.name.clone(),
                        VarStorage::Frame {
                            offset: x_off,
                            ty: x_ty,
                        },
                    );
                    true
                }
                Stmt::Assign(a) if is_channel_recv(&a.value).is_some() => {
                    let m = is_channel_recv(&a.value).unwrap();
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(&m.object)).unwrap_or(Type::I64);
                    let v = self.emit_coop_recv(&m.object, &elem_ty, suspends, frame);
                    if let Some(storage) = self.vars.get(&a.name).cloned() {
                        self.store_var(&storage, v);
                    }
                    true
                }
                Stmt::Expr(es) if is_channel_recv(&es.expr).is_some() => {
                    let m = is_channel_recv(&es.expr).unwrap();
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(&m.object)).unwrap_or(Type::I64);
                    self.emit_coop_recv(&m.object, &elem_ty, suspends, frame);
                    true
                }
                Stmt::FieldAssign(s)
                    if await_coop_call(&s.value, self.cooperative_leaves).is_some() =>
                {
                    let (call, await_span) =
                        await_coop_call(&s.value, self.cooperative_leaves).unwrap();
                    let obj_type = self.ast_type_of(&s.object);
                    if let Some(class_name) = class_name_for_object_type(&obj_type)
                        && let Some(layout) = self.class_layouts.get(&class_name).cloned()
                        && let Some(idx) = layout.iter().position(|(n, _)| n == &s.field)
                    {
                        let field_ty = layout[idx].1.clone();
                        let value_ty = self.ast_type_of(&s.value);
                        let result = self
                            .emit_coop_call_await(
                                call,
                                await_span,
                                None,
                                Some(value_ty.clone()),
                                suspends,
                                frame,
                            )
                            .expect("field assignment call-await requires a result value");

                        let ptr = self.emit_expr(&s.object);
                        self.emit_push_root(ptr);
                        if self.build_mode == BuildMode::Debug {
                            self.emit_nil_check(ptr, s.object.span(), &s.field);
                        }

                        let val = self.coerce_to_target(result, &value_ty, &field_ty);
                        let offset = (idx as i32 + 1) * 8;
                        self.builder.ins().store(MemFlags::new(), val, ptr, offset);

                        self.emit_pop_roots_n(1);
                        self.gc_root_count -= 1;
                    } else {
                        self.emit_coop_call_await(call, await_span, None, None, suspends, frame);
                    }
                    true
                }
                Stmt::IndexAssign(s)
                    if await_coop_call(&s.value, self.cooperative_leaves).is_some() =>
                {
                    let (call, await_span) =
                        await_coop_call(&s.value, self.cooperative_leaves).unwrap();
                    let elem_ty = array_element_type(&self.ast_type_of(&s.array));
                    let value_ty = self.ast_type_of(&s.value);
                    let result = self
                        .emit_coop_call_await(
                            call,
                            await_span,
                            None,
                            Some(value_ty.clone()),
                            suspends,
                            frame,
                        )
                        .expect("index assignment call-await requires a result value");

                    let arr = self.emit_expr(&s.array);
                    self.emit_push_root(arr);
                    let idx = self.emit_expr(&s.index);
                    let val = self.coerce_to_target(result, &value_ty, &elem_ty);
                    let word = self.coerce_to_i64(val, &elem_ty);
                    let set_id = self.func_ids["willow_array_set"];
                    let set_ref = self.module.declare_func_in_func(set_id, self.builder.func);
                    self.builder.ins().call(set_ref, &[arr, idx, word]);

                    self.emit_pop_roots_n(1);
                    self.gc_root_count -= 1;
                    true
                }
                Stmt::Return(r)
                    if r.value
                        .as_ref()
                        .and_then(|value| await_coop_call(value, self.cooperative_leaves))
                        .is_some() =>
                {
                    let value = r.value.as_ref().unwrap();
                    let (call, await_span) = await_coop_call(value, self.cooperative_leaves)
                        .expect("return call-await guard matched");
                    let result_ty = self.ast_type_of(value);
                    let result = self
                        .emit_coop_call_await(
                            call,
                            await_span,
                            None,
                            Some(result_ty),
                            suspends,
                            frame,
                        )
                        .expect("return call-await requires a result value");
                    if let Some(off) = result_offset {
                        self.builder
                            .ins()
                            .store(MemFlags::new(), result, frame, off);
                    }
                    let ready = self.builder.ins().iconst(types::I32, 1);
                    self.builder.ins().return_(&[ready]);
                    self.terminated = true;
                    false
                }
                Stmt::Return(r) => {
                    if let (Some(off), Some(v)) = (result_offset, &r.value) {
                        let val = self.emit_expr(v);
                        self.builder.ins().store(MemFlags::new(), val, frame, off);
                    }
                    let ready = self.builder.ins().iconst(types::I32, 1);
                    self.builder.ins().return_(&[ready]);
                    self.terminated = true;
                    false
                }
                Stmt::If(s) => {
                    let cond = self.emit_expr(&s.cond);
                    let then_b = self.builder.create_block();
                    let else_b = self.builder.create_block();
                    let join_b = self.builder.create_block();
                    self.builder.ins().brif(cond, then_b, &[], else_b, &[]);
                    self.builder.switch_to_block(then_b);
                    self.terminated = false;
                    let saved_vars = self.vars.clone();
                    let saved_roots = self.gc_root_count;
                    let then_falls =
                        self.emit_coop_stmts(&s.then_block.stmts, suspends, frame, result_offset);
                    if then_falls {
                        self.builder.ins().jump(join_b, &[]);
                    }
                    self.vars = saved_vars.clone();
                    self.gc_root_count = saved_roots;

                    self.builder.switch_to_block(else_b);
                    self.terminated = false;
                    let else_falls = if let Some(eb) = &s.else_block {
                        self.emit_coop_stmts(&eb.stmts, suspends, frame, result_offset)
                    } else {
                        true
                    };
                    if else_falls {
                        self.builder.ins().jump(join_b, &[]);
                    }
                    self.vars = saved_vars;
                    self.gc_root_count = saved_roots;

                    if then_falls || else_falls {
                        self.builder.switch_to_block(join_b);
                        self.terminated = false;
                        true
                    } else {
                        self.terminated = true;
                        false
                    }
                }
                Stmt::While(s) => {
                    let header = self.builder.create_block();
                    let body_b = self.builder.create_block();
                    let exit_b = self.builder.create_block();
                    self.builder.ins().jump(header, &[]);
                    self.builder.switch_to_block(header);
                    let cond = self.emit_expr(&s.cond);
                    self.builder.ins().brif(cond, body_b, &[], exit_b, &[]);
                    self.builder.switch_to_block(body_b);
                    self.terminated = false;
                    let saved_vars = self.vars.clone();
                    let saved_roots = self.gc_root_count;
                    let body_falls =
                        self.emit_coop_stmts(&s.body.stmts, suspends, frame, result_offset);
                    if body_falls {
                        self.builder.ins().jump(header, &[]);
                    }
                    self.vars = saved_vars;
                    self.gc_root_count = saved_roots;
                    self.builder.switch_to_block(exit_b);
                    self.terminated = false;
                    true
                }
                Stmt::For(s) => self.emit_coop_for(s, suspends, frame, result_offset),
                _ => {
                    self.emit_stmt(stmt);
                    !self.terminated
                }
            };

            if !falls_through {
                return false;
            }
        }
        true
    }

    fn emit_coop_for(
        &mut self,
        s: &ForStmt,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
        result_offset: Option<i32>,
    ) -> bool {
        if let Expr::Range(range) = &s.iterable {
            return self.emit_coop_range_for(s, range, suspends, frame, result_offset);
        }
        // A `Range<i64>` held as a value (variable/call), not an inline literal.
        if matches!(self.ast_type_of(&s.iterable), Type::Generic(ref n, _) if n == "Range") {
            return self.emit_coop_range_for_value(s, suspends, frame, result_offset);
        }

        let iterable_ty = self
            .async_local_types
            .get(&s.iter_frame_key())
            .cloned()
            .unwrap_or_else(|| self.ast_type_of(&s.iterable));
        let elem_ty = self
            .async_local_types
            .get(&s.name_span)
            .cloned()
            .unwrap_or_else(|| array_element_type(&iterable_ty));
        let iter_off = self.async_frame_offsets[&s.iter_frame_key()];
        let index_off = self.async_frame_offsets[&s.index_frame_key()];
        let item_off = (s.name != "_").then(|| self.async_frame_offsets[&s.name_span]);

        let arr = self.emit_expr(&s.iterable);
        self.builder
            .ins()
            .store(MemFlags::new(), arr, frame, iter_off);
        let zero = self.builder.ins().iconst(types::I64, 0);
        self.builder
            .ins()
            .store(MemFlags::new(), zero, frame, index_off);

        let header = self.builder.create_block();
        let body_b = self.builder.create_block();
        let exit_b = self.builder.create_block();
        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let arr =
            self.builder
                .ins()
                .load(clif_type(&iterable_ty), MemFlags::new(), frame, iter_off);
        let idx = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), frame, index_off);
        let len_id = self.func_ids["willow_array_len"];
        let len_ref = self.module.declare_func_in_func(len_id, self.builder.func);
        let len_call = self.builder.ins().call(len_ref, &[arr]);
        let len = self.builder.inst_results(len_call)[0];
        let keep_going = self.builder.ins().icmp(IntCC::SignedLessThan, idx, len);
        self.builder
            .ins()
            .brif(keep_going, body_b, &[], exit_b, &[]);

        self.builder.switch_to_block(body_b);
        self.terminated = false;
        let saved_vars = self.vars.clone();
        let saved_roots = self.gc_root_count;
        if let Some(off) = item_off {
            let get_id = self.func_ids["willow_array_get"];
            let get_ref = self.module.declare_func_in_func(get_id, self.builder.func);
            let call = self.builder.ins().call(get_ref, &[arr, idx]);
            let word = self.builder.inst_results(call)[0];
            let item = self.coerce_i64_to(word, &elem_ty);
            self.builder.ins().store(MemFlags::new(), item, frame, off);
            self.vars.insert(
                s.name.clone(),
                VarStorage::Frame {
                    offset: off,
                    ty: elem_ty,
                },
            );
        }

        let body_falls = self.emit_coop_stmts(&s.body.stmts, suspends, frame, result_offset);
        if body_falls {
            let idx = self
                .builder
                .ins()
                .load(types::I64, MemFlags::new(), frame, index_off);
            let one = self.builder.ins().iconst(types::I64, 1);
            let next = self.builder.ins().iadd(idx, one);
            self.builder
                .ins()
                .store(MemFlags::new(), next, frame, index_off);
            self.builder.ins().jump(header, &[]);
        }
        self.vars = saved_vars;
        self.gc_root_count = saved_roots;
        self.builder.switch_to_block(exit_b);
        self.terminated = false;
        true
    }

    fn emit_coop_range_for(
        &mut self,
        s: &ForStmt,
        range: &RangeExpr,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
        result_offset: Option<i32>,
    ) -> bool {
        let start = self.emit_expr(&range.start);
        let end = self.emit_expr(&range.end);
        self.emit_coop_range_for_bounds(s, start, end, suspends, frame, result_offset)
    }

    /// Cooperative `for` over a `Range<i64>` VALUE: load its bounds from the heap
    /// object, then drive the same frame-backed counting loop as the literal
    /// form (the bounds are copied into I64 frame slots, so they survive awaits).
    fn emit_coop_range_for_value(
        &mut self,
        s: &ForStmt,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
        result_offset: Option<i32>,
    ) -> bool {
        let ptr = self.emit_expr(&s.iterable);
        let start = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let end = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        self.emit_coop_range_for_bounds(s, start, end, suspends, frame, result_offset)
    }

    fn emit_coop_range_for_bounds(
        &mut self,
        s: &ForStmt,
        start: cranelift_codegen::ir::Value,
        end: cranelift_codegen::ir::Value,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
        result_offset: Option<i32>,
    ) -> bool {
        let end_off = self.async_frame_offsets[&s.iter_frame_key()];
        let current_off = self.async_frame_offsets[&s.index_frame_key()];
        let item_off = (s.name != "_").then(|| self.async_frame_offsets[&s.name_span]);

        self.builder
            .ins()
            .store(MemFlags::new(), start, frame, current_off);
        self.builder
            .ins()
            .store(MemFlags::new(), end, frame, end_off);

        let header = self.builder.create_block();
        let body_b = self.builder.create_block();
        let exit_b = self.builder.create_block();
        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let current = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), frame, current_off);
        let end = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), frame, end_off);
        let keep_going = self.builder.ins().icmp(IntCC::SignedLessThan, current, end);
        self.builder
            .ins()
            .brif(keep_going, body_b, &[], exit_b, &[]);

        self.builder.switch_to_block(body_b);
        self.terminated = false;
        let saved_vars = self.vars.clone();
        let saved_roots = self.gc_root_count;
        if let Some(off) = item_off {
            self.builder
                .ins()
                .store(MemFlags::new(), current, frame, off);
            self.vars.insert(
                s.name.clone(),
                VarStorage::Frame {
                    offset: off,
                    ty: Type::I64,
                },
            );
        }

        let body_falls = self.emit_coop_stmts(&s.body.stmts, suspends, frame, result_offset);
        if body_falls {
            let current = self
                .builder
                .ins()
                .load(types::I64, MemFlags::new(), frame, current_off);
            let one = self.builder.ins().iconst(types::I64, 1);
            let next = self.builder.ins().iadd(current, one);
            self.builder
                .ins()
                .store(MemFlags::new(), next, frame, current_off);
            self.builder.ins().jump(header, &[]);
        }
        self.vars = saved_vars;
        self.gc_root_count = saved_roots;
        self.builder.switch_to_block(exit_b);
        self.terminated = false;
        true
    }

    fn emit_block(&mut self, block: &Block) {
        let saved_vars = self.vars.clone();
        let gc_roots_before = self.gc_root_count;

        for stmt in &block.stmts {
            if self.terminated {
                break;
            }
            self.emit_stmt(stmt);
        }

        // Pop any GC roots introduced by this block before the vars go out of scope.
        if !self.terminated {
            let block_roots = self.gc_root_count - gc_roots_before;
            if block_roots > 0 {
                self.emit_pop_roots_n(block_roots);
            }
        }
        // Restore scope: gc_root_count goes back to what it was before the block
        // (in the terminated path the return handler already popped all roots).
        self.gc_root_count = gc_roots_before;
        self.vars = saved_vars;
    }

    fn emit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => {
                // With an interface annotation, a class initializer is boxed.
                let val = match &s.ty {
                    Some(target) => self.emit_expr_coerced(&s.init, &target.clone()),
                    None => self.emit_expr(&s.init),
                };
                // `_` is the wildcard name: evaluate for side effects but don't bind.
                if s.name == "_" {
                    return;
                }
                let ast_ty =
                    s.ty.clone()
                        .or_else(|| self.async_local_types.get(&s.span).cloned())
                        .unwrap_or_else(|| self.ast_type_of_init(&s.init));
                // In an async fn, a GC-managed local that is part of the frame
                // layout lives in the heap frame so it survives `await`
                // (willow-lpn.5b). The frame is already a GC root, so the local
                // needs no separate shadow-stack root.
                // Frame-back any local that has a frame offset. For eager async
                // only GC-managed locals get offsets (setup_async_frame); the
                // cooperative poll-fn path also assigns offsets to non-GC locals
                // so they survive suspension (willow-lpn.5.3 slice 3b). Non-GC
                // slots are not in the frame's GC mask, so they are not traced.
                if let Some(&offset) = self.async_frame_offsets.get(&s.span) {
                    let base = self
                        .async_frame
                        .expect("frame-backed local requires an allocated async frame");
                    self.builder.ins().store(MemFlags::new(), val, base, offset);
                    self.vars.insert(
                        s.name.clone(),
                        VarStorage::Frame {
                            offset,
                            ty: ast_ty.clone(),
                        },
                    );
                    return;
                }
                let storage = if is_gc_managed(&ast_ty, self.enum_infos) {
                    // GC-managed types: store in a stack slot so that the GC root
                    // slot and the variable slot are the SAME memory.  If we used
                    // an SSA variable for the value and a separate stack slot for
                    // the root, a reassignment (Stmt::Assign) would update the SSA
                    // variable but leave the root slot stale, allowing the GC to
                    // see old (possibly freed) pointers and collect the live new one.
                    let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        8,
                        0,
                    ));
                    self.builder.ins().stack_store(val, slot, 0);
                    let ptr_ty = self.module.target_config().pointer_type();
                    let addr = self.builder.ins().stack_addr(ptr_ty, slot, 0);
                    let push_id = self.func_ids["willow_push_root"];
                    let push_ref = self.module.declare_func_in_func(push_id, self.builder.func);
                    self.builder.ins().call(push_ref, &[addr]);
                    self.gc_root_count += 1;
                    VarStorage::Stack {
                        slot,
                        ty: ast_ty.clone(),
                    }
                } else {
                    let ty = clif_type(&ast_ty);
                    let var = self.builder.declare_var(ty);
                    self.builder.def_var(var, val);
                    VarStorage::Value {
                        var,
                        ty: ast_ty.clone(),
                    }
                };
                self.vars.insert(s.name.clone(), storage);
            }
            Stmt::Assign(s) => {
                if let Some(storage) = self.vars.get(&s.name).cloned() {
                    let target_ty = storage.ty().clone();
                    let val = self.emit_expr_coerced(&s.value, &target_ty);
                    self.store_var(&storage, val);
                }
            }
            Stmt::FieldAssign(s) => {
                let ptr = self.emit_expr(&s.object);
                if self.build_mode == BuildMode::Debug {
                    self.emit_nil_check(ptr, s.object.span(), &s.field);
                }
                let obj_type = self.ast_type_of(&s.object);
                if let Some(class_name) = class_name_for_object_type(&obj_type) {
                    if let Some(layout) = self.class_layouts.get(&class_name).cloned() {
                        if let Some(idx) = layout.iter().position(|(n, _)| n == &s.field) {
                            // Word 0 is type_id; fields start at word 1 → offset = (idx + 1) * 8.
                            let offset = (idx as i32 + 1) * 8;
                            // Box a class value when the field's type is an interface.
                            let field_ty = layout[idx].1.clone();
                            let val = self.emit_expr_coerced(&s.value, &field_ty);
                            self.builder.ins().store(MemFlags::new(), val, ptr, offset);
                        }
                    }
                }
            }
            Stmt::IndexAssign(s) => {
                // Null and out-of-bounds are checked inside `willow_array_set`.
                let arr = self.emit_expr(&s.array);
                // Root the array while the value expression is evaluated — it may
                // allocate and trigger a collection.
                self.emit_push_root(arr);
                let idx = self.emit_expr(&s.index);
                let elem_ty = array_element_type(&self.ast_type_of(&s.array));
                // Box a class value when the array's element type is an interface.
                let val = self.emit_expr_coerced(&s.value, &elem_ty);
                let word = self.coerce_to_i64(val, &elem_ty);
                let set_id = self.func_ids["willow_array_set"];
                let set_ref = self.module.declare_func_in_func(set_id, self.builder.func);
                self.builder.ins().call(set_ref, &[arr, idx, word]);
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
            }
            Stmt::If(s) => self.emit_if(s),
            Stmt::While(s) => self.emit_while(s),
            Stmt::For(s) => self.emit_for(s),
            Stmt::Return(s) => {
                // `fn main() -> Result<void, E>`: returns are turned into an exit
                // (Err -> report + non-zero; Ok / bare return -> exit 0), since
                // `willow_user_main` is void (willow-exg).
                if self.main_result_err_ty.is_some() {
                    match &s.value {
                        Some(val_expr) if is_zero_arg_result_ok(val_expr) => {
                            // `return Result::Ok();` — success, no construction.
                            if self.gc_root_count > 0 {
                                self.emit_pop_roots_n(self.gc_root_count);
                            }
                            self.builder.ins().return_(&[]);
                        }
                        Some(val_expr) => {
                            let result = self.emit_expr(val_expr);
                            self.emit_main_result_exit(result);
                        }
                        None => {
                            if self.gc_root_count > 0 {
                                self.emit_pop_roots_n(self.gc_root_count);
                            }
                            self.builder.ins().return_(&[]);
                        }
                    }
                    self.terminated = true;
                    return;
                }
                if self.is_async {
                    let future = if let Some(val_expr) = &s.value {
                        if self.return_type == Type::Void {
                            self.emit_expr(val_expr);
                            self.emit_ready_future_void()
                        } else {
                            let return_type = self.return_type.clone();
                            let val = self.emit_expr(val_expr);
                            self.emit_ready_future(&return_type, val)
                        }
                    } else {
                        self.emit_ready_future_void()
                    };
                    if self.gc_root_count > 0 {
                        self.emit_pop_roots_n(self.gc_root_count);
                    }
                    self.builder.ins().return_(&[future]);
                } else {
                    if let Some(val_expr) = &s.value {
                        // Evaluate the return value BEFORE popping roots (it may load from GC objects).
                        let target = self.return_type.clone();
                        let val = self.emit_expr_coerced(val_expr, &target);
                        if self.gc_root_count > 0 {
                            self.emit_pop_roots_n(self.gc_root_count);
                        }
                        self.builder.ins().return_(&[val]);
                    } else {
                        if self.gc_root_count > 0 {
                            self.emit_pop_roots_n(self.gc_root_count);
                        }
                        self.builder.ins().return_(&[]);
                    }
                }
                self.terminated = true;
            }
            Stmt::Expr(s) => {
                self.emit_expr(&s.expr);
            }
        }
    }

    fn emit_if(&mut self, s: &IfStmt) {
        let cond = self.emit_expr(&s.cond);

        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        self.builder
            .ins()
            .brif(cond, then_block, &[], else_block, &[]);

        // then branch
        self.builder.switch_to_block(then_block);
        self.builder.seal_block(then_block);
        let outer_terminated = self.terminated;
        self.terminated = false;
        self.emit_block(&s.then_block);
        let then_terminated = self.terminated;
        if !self.terminated {
            self.builder.ins().jump(merge_block, &[]);
        }

        // else branch
        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        self.terminated = false;
        if let Some(else_b) = &s.else_block {
            self.emit_block(else_b);
        }
        let else_terminated = self.terminated;
        if !self.terminated {
            self.builder.ins().jump(merge_block, &[]);
        }

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.terminated = outer_terminated || (then_terminated && else_terminated);
    }

    fn emit_while(&mut self, s: &WhileStmt) {
        let header = self.builder.create_block();
        let body_block = self.builder.create_block();
        let exit_block = self.builder.create_block();

        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let cond = self.emit_expr(&s.cond);
        self.builder
            .ins()
            .brif(cond, body_block, &[], exit_block, &[]);

        self.builder.switch_to_block(body_block);
        self.builder.seal_block(body_block);
        self.terminated = false;
        self.emit_block(&s.body);
        if !self.terminated {
            self.builder.ins().jump(header, &[]);
        }

        self.builder.seal_block(header);
        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(exit_block);
        self.terminated = false;
    }

    fn emit_for(&mut self, s: &ForStmt) {
        if let Expr::Range(range) = &s.iterable {
            self.emit_range_for(s, range);
            return;
        }

        let saved_vars = self.vars.clone();
        let roots_before = self.gc_root_count;
        let iterable_ty = self.ast_type_of(&s.iterable);
        // Iterating a `Range<i64>` held as a value.
        if matches!(&iterable_ty, Type::Generic(n, _) if n == "Range") {
            self.emit_range_for_value(s);
            self.vars = saved_vars;
            self.gc_root_count = roots_before;
            return;
        }
        let elem_ty = array_element_type(&iterable_ty);

        let arr = self.emit_expr(&s.iterable);
        self.emit_push_root(arr);

        let idx_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        let zero = self.builder.ins().iconst(types::I64, 0);
        self.builder.ins().stack_store(zero, idx_slot, 0);

        if s.name != "_" {
            let elem_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                0,
            ));
            if is_gc_managed(&elem_ty, self.enum_infos) {
                let nil = self.builder.ins().iconst(types::I64, 0);
                self.builder.ins().stack_store(nil, elem_slot, 0);
                self.emit_push_root_slot(elem_slot);
            }
            self.vars.insert(
                s.name.clone(),
                VarStorage::Stack {
                    slot: elem_slot,
                    ty: elem_ty.clone(),
                },
            );
        }

        let header = self.builder.create_block();
        let body_block = self.builder.create_block();
        let exit_block = self.builder.create_block();
        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let idx = self.builder.ins().stack_load(types::I64, idx_slot, 0);
        let len_id = self.func_ids["willow_array_len"];
        let len_ref = self.module.declare_func_in_func(len_id, self.builder.func);
        let len_call = self.builder.ins().call(len_ref, &[arr]);
        let len = self.builder.inst_results(len_call)[0];
        let keep_going = self.builder.ins().icmp(IntCC::SignedLessThan, idx, len);
        self.builder
            .ins()
            .brif(keep_going, body_block, &[], exit_block, &[]);

        self.builder.switch_to_block(body_block);
        self.builder.seal_block(body_block);
        if let Some(VarStorage::Stack { slot, .. }) = self.vars.get(&s.name).cloned() {
            let get_id = self.func_ids["willow_array_get"];
            let get_ref = self.module.declare_func_in_func(get_id, self.builder.func);
            let call = self.builder.ins().call(get_ref, &[arr, idx]);
            let word = self.builder.inst_results(call)[0];
            let elem = self.coerce_i64_to(word, &elem_ty);
            self.builder.ins().stack_store(elem, slot, 0);
        }
        self.terminated = false;
        self.emit_block(&s.body);
        if !self.terminated {
            let idx = self.builder.ins().stack_load(types::I64, idx_slot, 0);
            let one = self.builder.ins().iconst(types::I64, 1);
            let next = self.builder.ins().iadd(idx, one);
            self.builder.ins().stack_store(next, idx_slot, 0);
            self.builder.ins().jump(header, &[]);
        }

        self.builder.seal_block(header);
        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(exit_block);
        let loop_roots = self.gc_root_count - roots_before;
        if loop_roots > 0 {
            self.emit_pop_roots_n(loop_roots);
        }
        self.gc_root_count = roots_before;
        self.vars = saved_vars;
        self.terminated = false;
    }

    /// Materialize a `Range<i64>` value: a 2-word GC heap object `[start, end]`
    /// (both `i64`, so no GC-ref slots). Used when a range is held as a value
    /// rather than driven inline by a `for` loop.
    fn emit_range_value(&mut self, range: &RangeExpr) -> cranelift_codegen::ir::Value {
        // start/end are i64 scalars (not GC), computed before the only allocation,
        // so they survive it in registers without rooting.
        let start = self.emit_expr(&range.start);
        let end = self.emit_expr(&range.end);
        let size = self.builder.ins().iconst(types::I64, 16);
        let mask = self.builder.ins().iconst(types::I64, 0);
        let alloc_id = self.func_ids["willow_alloc_typed"];
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let ptr = self.builder.inst_results(call)[0];
        self.builder.ins().store(MemFlags::new(), start, ptr, 0i32);
        self.builder.ins().store(MemFlags::new(), end, ptr, 8i32);
        ptr
    }

    fn emit_range_for(&mut self, s: &ForStmt, range: &RangeExpr) {
        let start = self.emit_expr(&range.start);
        let end = self.emit_expr(&range.end);
        self.emit_range_for_bounds(s, start, end);
    }

    /// Iterate a `Range<i64>` VALUE (a variable or call result, not an inline
    /// literal): load its `start`/`end` words and drive the same counting loop.
    fn emit_range_for_value(&mut self, s: &ForStmt) {
        let ptr = self.emit_expr(&s.iterable);
        let start = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let end = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        self.emit_range_for_bounds(s, start, end);
    }

    fn emit_range_for_bounds(
        &mut self,
        s: &ForStmt,
        start: cranelift_codegen::ir::Value,
        end: cranelift_codegen::ir::Value,
    ) {
        let saved_vars = self.vars.clone();

        let current_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        let end_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        self.builder.ins().stack_store(start, current_slot, 0);
        self.builder.ins().stack_store(end, end_slot, 0);

        if s.name != "_" {
            let elem_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                0,
            ));
            self.vars.insert(
                s.name.clone(),
                VarStorage::Stack {
                    slot: elem_slot,
                    ty: Type::I64,
                },
            );
        }

        let header = self.builder.create_block();
        let body_block = self.builder.create_block();
        let exit_block = self.builder.create_block();
        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let current = self.builder.ins().stack_load(types::I64, current_slot, 0);
        let end = self.builder.ins().stack_load(types::I64, end_slot, 0);
        let keep_going = self.builder.ins().icmp(IntCC::SignedLessThan, current, end);
        self.builder
            .ins()
            .brif(keep_going, body_block, &[], exit_block, &[]);

        self.builder.switch_to_block(body_block);
        self.builder.seal_block(body_block);
        if let Some(VarStorage::Stack { slot, .. }) = self.vars.get(&s.name).cloned() {
            self.builder.ins().stack_store(current, slot, 0);
        }
        self.terminated = false;
        self.emit_block(&s.body);
        if !self.terminated {
            let current = self.builder.ins().stack_load(types::I64, current_slot, 0);
            let one = self.builder.ins().iconst(types::I64, 1);
            let next = self.builder.ins().iadd(current, one);
            self.builder.ins().stack_store(next, current_slot, 0);
            self.builder.ins().jump(header, &[]);
        }

        self.builder.seal_block(header);
        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(exit_block);
        self.vars = saved_vars;
        self.terminated = false;
    }

    /// Determine the AST type of a `let` initialiser, including full `Type::Fn` for
    /// named-function and lambda values so indirect calls later work correctly.
    /// Resolve the Willow AST type of an expression, handling FieldAccess and
    /// MethodCall by looking up class layouts and func_return_types.
    fn ast_type_of(&self, expr: &Expr) -> Type {
        match expr {
            // Static property read → its declared type (willow-qsqf), so e.g.
            // `println(C::prop)` selects the right print function.
            Expr::StaticField(s) => {
                let class = self.static_call_class_name(&s.class);
                self.static_storage
                    .get(&(class, s.field.clone()))
                    .map(|info| info.ty.clone())
                    .unwrap_or(Type::I64)
            }
            Expr::FieldAccess(obj, field_name, _) => {
                if let Some(class_name) = class_name_for_object_type(&self.ast_type_of(obj)) {
                    if let Some(layout) = self.class_layouts.get(&class_name) {
                        if let Some((_, ty)) = layout.iter().find(|(n, _)| n == field_name) {
                            return ty.clone();
                        }
                    }
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
                if m.method == "join" {
                    if let Some(result_ty) = join_handle_result_type(&obj_ty) {
                        return result_ty;
                    }
                }
                if m.method == "recv" {
                    if let Some(element_ty) = channel_element_type(&obj_ty) {
                        return element_ty;
                    }
                }
                if let Type::Array(elem) = &obj_ty {
                    match m.method.as_str() {
                        "len" => return Type::I64,
                        "pop" => return (**elem).clone(),
                        "push" => return Type::Void,
                        _ => {}
                    }
                }
                if let Type::Generic(name, margs) = &obj_ty {
                    if name == "Map" && margs.len() == 2 {
                        match m.method.as_str() {
                            "get" => {
                                return Type::Generic("Option".to_string(), vec![margs[1].clone()]);
                            }
                            "len" => return Type::I64,
                            "contains" => return Type::Bool,
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
                if let Type::Named(iface_name) = &obj_ty {
                    if let Some(iface) = self.interface_infos.get(iface_name) {
                        if let Some(method) = iface.methods.get(&m.method) {
                            return method.return_type.clone();
                        }
                    }
                }
                // Generic interface receiver (`Box<String>`): substitute the
                // interface's type parameters into the method's return type
                // (`fn get(self) -> T` -> `String`) (willow-1js.1).
                if let Type::Generic(iface_name, type_args) = &obj_ty {
                    if let Some(iface) = self.interface_infos.get(iface_name) {
                        if let Some(method) = iface.methods.get(&m.method) {
                            let map: HashMap<String, Type> = iface
                                .type_params
                                .iter()
                                .cloned()
                                .zip(type_args.iter().cloned())
                                .collect();
                            return crate::semantic::symbols::substitute_type(
                                &method.return_type,
                                &map,
                            );
                        }
                    }
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
                if let Some(enum_info) = self.enum_infos.get(class_name.as_str()) {
                    if !enum_info.type_params.is_empty() {
                        if let Some(variant) =
                            enum_info.variants.iter().find(|v| v.name == s.method)
                        {
                            // Infer type args: for each type parameter, find which payload position
                            // uses it and take the type of the corresponding argument.
                            let type_args: Vec<Type> =
                                enum_info
                                    .type_params
                                    .iter()
                                    .map(|param| {
                                        variant.payload_types.iter().zip(s.args.iter()).find_map(
                                        |(payload_ty, arg)| {
                                            if matches!(payload_ty, Type::Named(n) if n == param) {
                                                Some(self.ast_type_of(&arg.expr))
                                            } else {
                                                None
                                            }
                                        },
                                    )
                                    .unwrap_or(Type::Void)
                                    })
                                    .collect();
                            return Type::Generic(class_name.clone(), type_args);
                        }
                    }
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
                ast_type_of_expr(expr, &self.vars, self.func_return_types)
            }
            _ => ast_type_of_expr(expr, &self.vars, self.func_return_types),
        }
    }

    fn ast_type_of_init(&self, expr: &Expr) -> Type {
        match expr {
            // Static property read → its declared type (so `let x = C::prop`
            // gets the right storage clif type), willow-qsqf.
            Expr::StaticField(s) => {
                let class = self.static_call_class_name(&s.class);
                self.static_storage
                    .get(&(class, s.field.clone()))
                    .map(|info| info.ty.clone())
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

    fn emit_expr(&mut self, expr: &Expr) -> cranelift_codegen::ir::Value {
        match expr {
            Expr::Integer(n, _) => self.builder.ins().iconst(types::I64, *n),
            Expr::Float(f, _) => self.builder.ins().f64const(*f),
            Expr::Bool(b, _) => self.builder.ins().iconst(types::I8, if *b { 1 } else { 0 }),
            Expr::Nil(_) => self.builder.ins().iconst(types::I64, 0),
            Expr::String(value, _) => self.emit_string_literal(value),
            Expr::Var(name, _) => {
                // Local variable or function value?
                if let Some(storage) = self.vars.get(name.as_str()).cloned() {
                    return self.load_var(&storage);
                }
                // Named function used as a first-class value — emit its address.
                if let Some(&fid) = self.func_ids.get(name.as_str()) {
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    return self.builder.ins().func_addr(types::I64, fref);
                }
                // Should not reach here after type checking, but produce a safe zero.
                self.builder.ins().iconst(types::I64, 0)
            }
            Expr::Binary(b) => self.emit_binary(b),
            Expr::Unary(u) => self.emit_unary(u),
            Expr::Call(c) => self.emit_call(c),
            Expr::Print(arg, newline, _) => {
                let val = self.emit_expr(arg);
                let arg_ty_raw = self.ast_type_of(arg);
                // Unwrap Nullable so that printing a nil-checked T? behaves like T.
                let arg_ty = match arg_ty_raw {
                    Type::Nullable(inner) => *inner,
                    ty => ty,
                };
                let fn_name = match (arg_ty, newline) {
                    (Type::I64, false) => "willow_print_i64",
                    (Type::I64, true) => "willow_println_i64",
                    (Type::F64, false) => "willow_print_f64",
                    (Type::F64, true) => "willow_println_f64",
                    (Type::Bool, false) => "willow_print_bool",
                    (Type::Bool, true) => "willow_println_bool",
                    (Type::String, false) => "willow_print_string",
                    (Type::String, true) => "willow_println_string",
                    _ => "",
                };
                if !fn_name.is_empty() {
                    let fid = self.func_ids[fn_name];
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    self.builder.ins().call(fref, &[val]);
                }
                self.builder.ins().iconst(types::I8, 0)
            }
            Expr::Ternary(t) => self.emit_ternary(t),
            Expr::Range(r) => self.emit_range_value(r),
            // Lambda: emit the address of the pre-compiled private function.
            Expr::Lambda(l) => {
                if let Some(name) = self.lambda_names.get(&l.span) {
                    if let Some(&fid) = self.func_ids.get(name.as_str()) {
                        let fref = self.module.declare_func_in_func(fid, self.builder.func);
                        return self.builder.ins().func_addr(types::I64, fref);
                    }
                }
                self.builder.ins().iconst(types::I64, 0)
            }
            // Field/method access: codegen deferred to willow-jbf
            Expr::FieldAccess(obj, field_name, _) => self.emit_field_access(obj, field_name),
            Expr::MethodCall(m) => self.emit_method_call(m),
            Expr::ObjectLiteral(o) => self.emit_object_literal(o),
            Expr::StaticCall(s) => self.emit_static_call(s),
            Expr::StaticField(s) => self.emit_static_field_read(&s.class, &s.field),
            Expr::Spawn(s) => self.emit_spawn(s),
            Expr::Await(a) => self.emit_await(a),
            Expr::Select(s) => {
                self.emit_select(s);
                self.builder.ins().iconst(types::I8, 0)
            }
            Expr::Match(m) => self.emit_match(m),
            Expr::TryPropagate(inner, _) => self.emit_try_propagate(inner),
            Expr::ArrayLiteral(elements, _) => {
                let elem_ty = elements
                    .first()
                    .map(|e| self.ast_type_of(e))
                    .unwrap_or(Type::Void);
                self.emit_array_literal(elements, &elem_ty)
            }
            Expr::Index(arr, index, _) => {
                // Null and out-of-bounds are checked inside `willow_array_get`,
                // which aborts with a clear message.
                let elem_ty = array_element_type(&self.ast_type_of(arr));
                self.emit_index(arr, index, &elem_ty)
            }
        }
    }

    fn emit_string_literal(&mut self, value: &str) -> cranelift_codegen::ir::Value {
        if let Some(data_id) = self.string_literals.get(value) {
            // Load the address of the static raw bytes.
            let gv = self
                .module
                .declare_data_in_func(*data_id, self.builder.func);
            let ptr_ty = self.module.target_config().pointer_type();
            let bytes_ptr = self.builder.ins().global_value(ptr_ty, gv);
            // Call willow_string_literal to get (or create) a permanent WillowString.
            let len_val = self.builder.ins().iconst(types::I64, value.len() as i64);
            let fid = self.func_ids["willow_string_literal"];
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self.builder.ins().call(fref, &[bytes_ptr, len_val]);
            return self.builder.inst_results(call)[0];
        }
        self.builder.ins().iconst(types::I64, 0)
    }

    fn emit_binary(&mut self, b: &BinaryExpr) -> cranelift_codegen::ir::Value {
        let lhs = self.emit_expr(&b.lhs);
        let lty = self.ast_type_of(&b.lhs);
        let is_float = lty == Type::F64;
        match &b.op {
            BinOp::And => self.emit_short_circuit_and(lhs, &b.rhs),
            BinOp::Or => self.emit_short_circuit_or(lhs, &b.rhs),
            BinOp::Add => {
                if lty == Type::String {
                    // Root lhs before evaluating rhs: rhs evaluation may call
                    // willow_alloc_typed (e.g. concat or object allocation) which
                    // triggers a GC cycle.  Without rooting, an intermediate concat
                    // result held only in lhs's SSA register would be freed.
                    self.emit_push_root(lhs);
                    let rhs = self.emit_expr(&b.rhs);
                    // Root rhs before the concat call itself, which also allocates.
                    self.emit_push_root(rhs);
                    let fid = self.func_ids["willow_string_concat"];
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    let call = self.builder.ins().call(fref, &[lhs, rhs]);
                    let result = self.builder.inst_results(call)[0];
                    // Pop the two temporary roots (net gc_root_count change is 0).
                    self.emit_pop_roots_n(2);
                    self.gc_root_count -= 2;
                    result
                } else {
                    let rhs = self.emit_expr(&b.rhs);
                    if is_float {
                        self.builder.ins().fadd(lhs, rhs)
                    } else {
                        self.builder.ins().iadd(lhs, rhs)
                    }
                }
            }
            BinOp::Sub => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    self.builder.ins().fsub(lhs, rhs)
                } else {
                    self.builder.ins().isub(lhs, rhs)
                }
            }
            BinOp::Mul => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    self.builder.ins().fmul(lhs, rhs)
                } else {
                    self.builder.ins().imul(lhs, rhs)
                }
            }
            BinOp::Div => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    self.builder.ins().fdiv(lhs, rhs)
                } else {
                    self.builder.ins().sdiv(lhs, rhs)
                }
            }
            BinOp::Rem => {
                let rhs = self.emit_expr(&b.rhs);
                self.builder.ins().srem(lhs, rhs)
            }
            BinOp::Lt => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::LessThan, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedLessThan, lhs, rhs)
                }
            }
            BinOp::Le => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::LessThanOrEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedLessThanOrEqual, lhs, rhs)
                }
            }
            BinOp::Gt => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::GreaterThan, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedGreaterThan, lhs, rhs)
                }
            }
            BinOp::Ge => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::GreaterThanOrEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedGreaterThanOrEqual, lhs, rhs)
                }
            }
            BinOp::Eq => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::Equal, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::Equal, lhs, rhs)
                }
            }
            BinOp::Ne => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::NotEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::NotEqual, lhs, rhs)
                }
            }
        }
    }

    fn emit_short_circuit_and(
        &mut self,
        lhs: cranelift_codegen::ir::Value,
        rhs_expr: &Expr,
    ) -> cranelift_codegen::ir::Value {
        let result_var = self.builder.declare_var(types::I8);
        let rhs_block = self.builder.create_block();
        let false_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        self.builder
            .ins()
            .brif(lhs, rhs_block, &[], false_block, &[]);

        self.builder.switch_to_block(rhs_block);
        self.builder.seal_block(rhs_block);
        let rhs = self.emit_expr(rhs_expr);
        self.builder.def_var(result_var, rhs);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(false_block);
        self.builder.seal_block(false_block);
        let false_value = self.builder.ins().iconst(types::I8, 0);
        self.builder.def_var(result_var, false_value);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    fn emit_short_circuit_or(
        &mut self,
        lhs: cranelift_codegen::ir::Value,
        rhs_expr: &Expr,
    ) -> cranelift_codegen::ir::Value {
        let result_var = self.builder.declare_var(types::I8);
        let true_block = self.builder.create_block();
        let rhs_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        self.builder
            .ins()
            .brif(lhs, true_block, &[], rhs_block, &[]);

        self.builder.switch_to_block(true_block);
        self.builder.seal_block(true_block);
        let true_value = self.builder.ins().iconst(types::I8, 1);
        self.builder.def_var(result_var, true_value);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(rhs_block);
        self.builder.seal_block(rhs_block);
        let rhs = self.emit_expr(rhs_expr);
        self.builder.def_var(result_var, rhs);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    fn emit_unary(&mut self, u: &UnaryExpr) -> cranelift_codegen::ir::Value {
        let val = self.emit_expr(&u.expr);
        let ty = ast_type_of_expr(&u.expr, &self.vars, self.func_return_types);
        match &u.op {
            UnaryOp::Neg => {
                if ty == Type::F64 {
                    self.builder.ins().fneg(val)
                } else {
                    self.builder.ins().ineg(val)
                }
            }
            UnaryOp::Not => {
                let one = self.builder.ins().iconst(types::I8, 1);
                self.builder.ins().bxor(val, one)
            }
        }
    }

    fn emit_call(&mut self, c: &CallExpr) -> cranelift_codegen::ir::Value {
        if c.callee == "format" {
            return self.emit_format_call(c);
        }

        // Direct call to a known function.
        if let Some(&fid) = self.func_ids.get(&c.callee) {
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let modes = self.func_param_modes.get(&c.callee).cloned();
            let param_debug = self.func_param_debug.get(&c.callee).cloned();
            let param_types = self.fn_param_types(&c.callee);
            let has_reference_args = has_reference_args(modes.as_deref(), &c.args);
            let (args, temp_roots) = self.emit_call_args_rooted_coerced(
                Some(&c.callee),
                modes.as_deref(),
                param_debug.as_deref(),
                param_types.as_deref(),
                &c.args,
            );
            // Debug builds: record this call on the call-chain stack so a panic
            // inside the callee prints an ordered trace (willow-992h). Pushed
            // after args are evaluated (so nested calls nest correctly) and
            // popped right after the call returns.
            let pushed_frame = self.emit_callstack_push(&c.callee, c.span);
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            let result = if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
            if pushed_frame {
                self.emit_callstack_pop();
            }
            if has_reference_args {
                self.emit_debug_reference_call_clear();
            }
            self.emit_pop_roots_n(temp_roots);
            self.gc_root_count -= temp_roots;
            return result;
        }

        // panic(message) — call willow_panic and trap (noreturn).
        if c.callee == "panic" {
            let msg = c
                .args
                .first()
                .map(|a| self.emit_expr(&a.expr))
                .unwrap_or_else(|| {
                    let empty = self.emit_string_literal("explicit panic");
                    empty
                });
            // Debug builds report the panic source location via willow_panic_at;
            // release builds use the plain willow_panic (willow-4j6).
            if self.build_mode == BuildMode::Debug {
                let source_file = self.source_file.to_string();
                let file_ptr = self.emit_string_literal(&source_file);
                let line = self.builder.ins().iconst(types::I32, c.span.line as i64);
                let col = self.builder.ins().iconst(types::I32, c.span.col as i64);
                let fid = self.func_ids["willow_panic_at"];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[msg, file_ptr, line, col]);
            } else {
                let fid = self.func_ids["willow_panic"];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[msg]);
            }
            // Produce the (unreachable) result value BEFORE the trap: `trap`
            // terminates the block, so no instruction may follow it. willow_panic
            // is noreturn; the trap just gives the block a terminator.
            let result = self.builder.ins().iconst(types::I64, 0);
            self.builder.ins().trap(TrapCode::unwrap_user(1));
            self.terminated = true;
            return result;
        }

        if let Some(runtime_name) = builtin_call_runtime_name(&c.callee) {
            let fid = self.func_ids[runtime_name];
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let args: Vec<_> = c.args.iter().map(|a| self.emit_expr(&a.expr)).collect();
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            return if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
        }

        // Indirect call through a function-value local variable.
        if let Some(storage) = self.vars.get(&c.callee).cloned() {
            if let Type::Fn(param_types, ret_type) = storage.ty().clone() {
                let callee_val = self.load_var(&storage);
                let args: Vec<_> = c.args.iter().map(|a| self.emit_expr(&a.expr)).collect();

                // Build the Cranelift signature matching the function type.
                let mut sig = self.module.make_signature();
                for pt in &param_types {
                    sig.params.push(AbiParam::new(clif_type(pt)));
                }
                let ret_clif = clif_type(&ret_type);
                let has_return = *ret_type != Type::Void;
                if has_return {
                    sig.returns.push(AbiParam::new(ret_clif));
                }
                let sig_ref = self.builder.import_signature(sig);
                let call = self.builder.ins().call_indirect(sig_ref, callee_val, &args);
                let results = self.builder.inst_results(call);
                return if results.is_empty() {
                    self.builder.ins().iconst(types::I8, 0)
                } else {
                    results[0]
                };
            }
        }

        // Should not reach here after type checking.
        self.builder.ins().iconst(types::I64, 0)
    }

    /// Evaluate call arguments left-to-right, pushing each GC-managed argument
    /// value as a temporary root before evaluating subsequent arguments.
    /// Returns (arg_values, number_of_temporary_roots_pushed).
    /// The caller must call emit_pop_roots_n(temp_roots) + gc_root_count -= temp_roots
    /// immediately after emitting the call instruction.
    fn emit_call_args_rooted(
        &mut self,
        callee: Option<&str>,
        modes: Option<&[ParamMode]>,
        param_debug: Option<&[ParamDebug]>,
        args: &[CallArg],
    ) -> (Vec<cranelift_codegen::ir::Value>, usize) {
        self.emit_call_args_rooted_coerced(callee, modes, param_debug, None, args)
    }

    /// Like [`emit_call_args_rooted`] but, when `param_types` is provided, boxes
    /// each class argument passed to an interface-typed parameter (spec §9.3).
    fn emit_call_args_rooted_coerced(
        &mut self,
        callee: Option<&str>,
        modes: Option<&[ParamMode]>,
        param_debug: Option<&[ParamDebug]>,
        param_types: Option<&[Type]>,
        args: &[CallArg],
    ) -> (Vec<cranelift_codegen::ir::Value>, usize) {
        let mut values = Vec::with_capacity(args.len());
        let mut temp_roots = 0usize;

        for (idx, arg) in args.iter().enumerate() {
            let is_ref = matches!(
                modes.and_then(|m| m.get(idx)),
                Some(ParamMode::Reference { .. })
            );
            let (val, reference_roots) = if is_ref {
                self.emit_debug_reference_call_hook(callee, idx, arg, modes, param_debug);
                self.emit_reference_arg_address(arg)
            } else {
                let raw = self.emit_expr(&arg.expr);
                // Box a class argument when the parameter type is an interface.
                let coerced = match param_types.and_then(|pt| pt.get(idx)) {
                    Some(target) => {
                        let value_ty = self.ast_type_of(&arg.expr);
                        self.coerce_to_target(raw, &value_ty, target)
                    }
                    None => raw,
                };
                (coerced, 0)
            };
            values.push(val);
            temp_roots += reference_roots;

            // Root GC-managed arguments so later argument evaluations (which may
            // allocate and trigger GC) cannot collect them.
            if !is_ref {
                let arg_ty = self.ast_type_of_init(&arg.expr);
                if is_gc_managed(&arg_ty, self.enum_infos) {
                    self.emit_push_root(val);
                    temp_roots += 1;
                }
            }
        }

        (values, temp_roots)
    }

    fn emit_debug_reference_call_hook(
        &mut self,
        callee: Option<&str>,
        idx: usize,
        arg: &CallArg,
        modes: Option<&[ParamMode]>,
        param_debug: Option<&[ParamDebug]>,
    ) {
        if self.build_mode != BuildMode::Debug {
            return;
        }

        let ampersand_span = match &arg.mode {
            CallArgMode::Reference { ampersand_span } => *ampersand_span,
            CallArgMode::Value => return,
        };

        let param = param_debug.and_then(|params| params.get(idx));
        let param_mode = param
            .map(|param| &param.mode)
            .or_else(|| modes.and_then(|modes| modes.get(idx)));
        let mode = param_mode.map(reference_mode_name).unwrap_or("&");
        let param_name = param
            .map(|param| param.name.as_str())
            .unwrap_or("<unknown>");
        let param_type = param
            .map(|param| debug_type_name(&param.ty))
            .unwrap_or_else(|| "<unknown>".to_string());
        let callee = callee.unwrap_or("<unknown>");
        let place_kind = reference_place_kind(&arg.expr);
        let place_name = reference_place_name(&arg.expr);

        let source_file = self.source_file.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let line_val = self
            .builder
            .ins()
            .iconst(types::I32, ampersand_span.line as i64);
        let col_val = self
            .builder
            .ins()
            .iconst(types::I32, ampersand_span.col as i64);
        let callee_ptr = self.emit_string_literal(callee);
        let param_ptr = self.emit_string_literal(param_name);
        let param_type_ptr = self.emit_string_literal(&param_type);
        let mode_ptr = self.emit_string_literal(mode);
        let place_kind_ptr = self.emit_string_literal(place_kind);
        let place_name_ptr = self.emit_string_literal(&place_name);
        let hook_id = self.func_ids["willow_debug_reference_call"];
        let hook_ref = self.module.declare_func_in_func(hook_id, self.builder.func);
        self.builder.ins().call(
            hook_ref,
            &[
                file_ptr,
                line_val,
                col_val,
                callee_ptr,
                param_ptr,
                param_type_ptr,
                mode_ptr,
                place_kind_ptr,
                place_name_ptr,
            ],
        );
    }

    fn emit_debug_reference_call_clear(&mut self) {
        if self.build_mode != BuildMode::Debug {
            return;
        }
        let clear_id = self.func_ids["willow_debug_reference_call_clear"];
        let clear_ref = self
            .module
            .declare_func_in_func(clear_id, self.builder.func);
        self.builder.ins().call(clear_ref, &[]);
    }

    /// Address + length of a declared string literal's raw static UTF-8 bytes,
    /// without interning a (GC-heap) WillowString. `None` if the literal was not
    /// collected/declared.
    fn emit_static_str_bytes(
        &mut self,
        value: &str,
    ) -> Option<(cranelift_codegen::ir::Value, cranelift_codegen::ir::Value)> {
        let data_id = *self.string_literals.get(value)?;
        let gv = self.module.declare_data_in_func(data_id, self.builder.func);
        let ptr_ty = self.module.target_config().pointer_type();
        let bytes_ptr = self.builder.ins().global_value(ptr_ty, gv);
        let len = self.builder.ins().iconst(types::I64, value.len() as i64);
        Some((bytes_ptr, len))
    }

    /// Debug builds: push a call-chain frame (callee name + call-site location)
    /// before a user-function call. Returns `true` when a frame was pushed (so
    /// the caller knows to emit the matching pop). Passes raw static bytes (not
    /// WillowStrings) so the call stack does not allocate on the GC heap. Release
    /// builds are untouched (willow-992h).
    fn emit_callstack_push(&mut self, callee: &str, span: crate::diagnostics::Span) -> bool {
        if self.build_mode != BuildMode::Debug {
            return false;
        }
        let Some((name_ptr, name_len)) = self.emit_static_str_bytes(callee) else {
            return false;
        };
        let file = self.source_file.to_string();
        let Some((file_ptr, file_len)) = self.emit_static_str_bytes(&file) else {
            return false;
        };
        let line = self.builder.ins().iconst(types::I32, span.line as i64);
        let col = self.builder.ins().iconst(types::I32, span.col as i64);
        let push_id = self.func_ids["willow_callstack_push"];
        let push_ref = self.module.declare_func_in_func(push_id, self.builder.func);
        self.builder.ins().call(
            push_ref,
            &[name_ptr, name_len, file_ptr, file_len, line, col],
        );
        true
    }

    /// Debug builds: pop the most recent call-chain frame after a call returns.
    fn emit_callstack_pop(&mut self) {
        let pop_id = self.func_ids["willow_callstack_pop"];
        let pop_ref = self.module.declare_func_in_func(pop_id, self.builder.func);
        self.builder.ins().call(pop_ref, &[]);
    }

    fn emit_reference_arg_address(
        &mut self,
        arg: &CallArg,
    ) -> (cranelift_codegen::ir::Value, usize) {
        match &arg.expr {
            Expr::Var(name, _) => {
                let storage = self.vars.get(name.as_str()).cloned();
                match storage {
                    Some(VarStorage::Stack { slot, .. }) => {
                        let ptr_ty = self.module.target_config().pointer_type();
                        return (self.builder.ins().stack_addr(ptr_ty, slot, 0), 0);
                    }
                    Some(VarStorage::Value { var, ty }) => {
                        // Lazy promotion: this variable is being passed by &mut for the first
                        // time.  Promote it from a Cranelift SSA variable to a stack slot so
                        // the callee can write through the pointer and future reads see the
                        // updated value.
                        let ptr_ty = self.module.target_config().pointer_type();
                        let val = self.builder.use_var(var);
                        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            8,
                            0,
                        ));
                        self.builder.ins().stack_store(val, slot, 0);
                        let ty_clone = ty.clone();
                        self.vars
                            .insert(name.clone(), VarStorage::Stack { slot, ty: ty_clone });
                        return (self.builder.ins().stack_addr(ptr_ty, slot, 0), 0);
                    }
                    Some(VarStorage::ReferencePtr { var, .. }) => {
                        return (self.builder.use_var(var), 0);
                    }
                    Some(VarStorage::Frame { offset, .. }) => {
                        // Address of the frame slot (frame is GC-rooted, non-moving).
                        let base = self
                            .async_frame
                            .expect("frame-backed var requires an allocated async frame");
                        return (self.builder.ins().iadd_imm(base, offset as i64), 0);
                    }
                    None => {}
                }
            }
            Expr::FieldAccess(obj, field_name, span) => {
                return (self.emit_field_address(obj, field_name, *span), 0);
            }
            Expr::Index(array, index, _) => {
                return self.emit_array_element_address(array, index);
            }
            _ => {}
        }
        (self.builder.ins().iconst(types::I64, 0), 0)
    }

    fn emit_field_address(
        &mut self,
        obj: &Expr,
        field_name: &str,
        _span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        let ptr = self.emit_expr(obj);

        if self.build_mode == BuildMode::Debug {
            self.emit_nil_check(ptr, obj.span(), field_name);
        }

        let obj_type = self.ast_type_of(obj);
        if let Some(class_name) = class_name_for_object_type(&obj_type) {
            if let Some(layout) = self.class_layouts.get(&class_name) {
                if let Some(idx) = layout.iter().position(|(n, _)| n == field_name) {
                    let offset = (idx as i64 + 1) * 8;
                    return self.builder.ins().iadd_imm(ptr, offset);
                }
            }
        }
        self.builder.ins().iconst(types::I64, 0)
    }

    fn emit_array_element_address(
        &mut self,
        array: &Expr,
        index: &Expr,
    ) -> (cranelift_codegen::ir::Value, usize) {
        let arr = self.emit_expr(array);
        // Keep the array alive while evaluating the index and while the callee
        // reads/writes through the returned element slot pointer.
        self.emit_push_root(arr);
        let index = self.emit_expr(index);
        let addr_id = self.func_ids["willow_array_element_addr"];
        let addr_ref = self.module.declare_func_in_func(addr_id, self.builder.func);
        let call = self.builder.ins().call(addr_ref, &[arr, index]);
        (self.builder.inst_results(call)[0], 1)
    }

    fn emit_format_call(&mut self, c: &CallExpr) -> cranelift_codegen::ir::Value {
        let runtime_name = match c.args.first().map(|arg| &arg.expr) {
            Some(Expr::String(spec, _)) => match spec.as_str() {
                "{:.17g}" => "willow_format_f64_17g",
                "{:.16f}" => "willow_format_f64_16f",
                "{:.6f}" => "willow_format_f64_6f",
                _ => "",
            },
            _ => "",
        };
        if runtime_name.is_empty() || c.args.len() < 2 {
            return self.builder.ins().iconst(types::I64, 0);
        }

        let value = self.emit_expr(&c.args[1].expr);
        let fid = self.func_ids[runtime_name];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[value]);
        self.builder.inst_results(call)[0]
    }

    fn emit_object_literal(&mut self, o: &ObjectLiteralExpr) -> cranelift_codegen::ir::Value {
        let layout = match self.class_layouts.get(&o.class).cloned() {
            Some(l) => l,
            None => return self.builder.ins().iconst(types::I64, 0),
        };
        // Object layout: word 0 = type_id (i64), words 1..N = fields.
        let size = (layout.len() as i64 + 1) * 8;
        let size_val = self.builder.ins().iconst(types::I64, size);
        let ref_mask = gc_ref_mask_for_layout(&layout, self.enum_infos);
        let ref_mask_val = self.builder.ins().iconst(types::I64, ref_mask as i64);
        let alloc_id = self.func_ids["willow_alloc_typed"];
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self
            .builder
            .ins()
            .call(alloc_ref, &[size_val, ref_mask_val]);
        let ptr = self.builder.inst_results(call)[0];

        // Root ptr immediately: evaluating field initialiser expressions below
        // may trigger allocations and GC cycles before all fields are stored.
        // Without this root, GC could collect the partially-initialised object.
        self.emit_push_root(ptr);

        // Store the type_id at offset 0.
        let type_id = self.class_type_ids.get(&o.class).copied().unwrap_or(0);
        let type_id_val = self.builder.ins().iconst(types::I64, type_id);
        self.builder
            .ins()
            .store(MemFlags::new(), type_id_val, ptr, 0i32);

        // Store each field at offset (idx + 1) * 8 to leave word 0 for type_id.
        for field in &o.fields {
            if let Some(idx) = layout.iter().position(|(n, _)| n == &field.name) {
                let offset = (idx as i32 + 1) * 8;
                // Box a class value when the field's declared type is an interface.
                let field_ty = layout[idx].1.clone();
                let val = self.emit_expr_coerced(&field.value, &field_ty);
                self.builder.ins().store(MemFlags::new(), val, ptr, offset);
            }
        }

        // Pop the temporary construction root; the caller will root ptr via
        // its own let-binding or return value handling.
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;

        ptr
    }

    /// Emit a nil pointer check in debug builds.
    /// If `ptr` is null at runtime, calls `willow_nil_deref` with source location and
    /// `context` (field or method name) then traps. Otherwise execution continues.
    fn emit_nil_check(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        span: crate::diagnostics::Span,
        context: &str,
    ) {
        let zero = self.builder.ins().iconst(types::I64, 0);
        let is_nil = self.builder.ins().icmp(IntCC::Equal, ptr, zero);

        let nil_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_nil, nil_block, &[], ok_block, &[]);

        // ── nil branch: report and abort ──────────────────────────────────────
        self.builder.switch_to_block(nil_block);
        self.builder.seal_block(nil_block);

        let source_file = self.source_file.to_string();
        let context_owned = context.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let ctx_ptr = self.emit_string_literal(&context_owned);
        let line_val = self.builder.ins().iconst(types::I32, span.line as i64);
        let col_val = self.builder.ins().iconst(types::I32, span.col as i64);

        let nil_deref_id = self.func_ids["willow_nil_deref"];
        let nil_deref_ref = self
            .module
            .declare_func_in_func(nil_deref_id, self.builder.func);
        self.builder
            .ins()
            .call(nil_deref_ref, &[file_ptr, line_val, col_val, ctx_ptr]);
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        // ── ok branch: continue ───────────────────────────────────────────────
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
    }

    fn emit_field_access(&mut self, obj: &Expr, field_name: &str) -> cranelift_codegen::ir::Value {
        let ptr = self.emit_expr(obj);

        // Debug build: guard against nil dereference with a source-aware runtime error.
        if self.build_mode == BuildMode::Debug {
            let span = obj.span();
            self.emit_nil_check(ptr, span, field_name);
        }

        let obj_type = self.ast_type_of(obj);
        // Range<i64> bounds: word 0 = start, word 1 = end.
        if matches!(&obj_type, Type::Generic(n, _) if n == "Range") {
            let offset = if field_name == "end" { 8i32 } else { 0i32 };
            return self
                .builder
                .ins()
                .load(types::I64, MemFlags::new(), ptr, offset);
        }
        if let Some(class_name) = class_name_for_object_type(&obj_type) {
            if let Some(layout) = self.class_layouts.get(&class_name).cloned() {
                if let Some(idx) = layout.iter().position(|(n, _)| n == field_name) {
                    // Word 0 is type_id; fields start at word 1 → offset = (idx + 1) * 8.
                    let offset = (idx as i32 + 1) * 8;
                    let (_, field_ty) = &layout[idx];
                    let load_ty = clif_type(field_ty);
                    return self
                        .builder
                        .ins()
                        .load(load_ty, MemFlags::new(), ptr, offset);
                }
            }
        }
        self.builder.ins().iconst(types::I64, 0)
    }

    /// Emit code for Option<T> and Result<T,E> built-in helper methods.
    /// Returns Some(value) if the method was handled, None to fall through to
    /// the regular class-method dispatch.
    fn emit_option_result_method_call(
        &mut self,
        enum_ptr: cranelift_codegen::ir::Value,
        obj_ty: &Type,
        m: &MethodCallExpr,
    ) -> Option<cranelift_codegen::ir::Value> {
        // Tag layout: Ok/Some = tag 0, Err/None = tag 1 (declaration order).
        const SOME_TAG: i64 = 0;
        const NONE_TAG: i64 = 1;
        const OK_TAG: i64 = 0;
        const ERR_TAG: i64 = 1;

        match obj_ty {
            Type::Generic(name, args) if name == "Option" => {
                let inner_ty = args.first().cloned().unwrap_or(Type::Void);
                match m.method.as_str() {
                    "is_some" => {
                        let tag =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), enum_ptr, 0i32);
                        let some = self.builder.ins().iconst(types::I64, SOME_TAG);
                        Some(self.builder.ins().icmp(IntCC::Equal, tag, some))
                    }
                    "is_none" => {
                        let tag =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), enum_ptr, 0i32);
                        let none = self.builder.ins().iconst(types::I64, NONE_TAG);
                        Some(self.builder.ins().icmp(IntCC::Equal, tag, none))
                    }
                    "unwrap" => {
                        let msg =
                            self.emit_string_literal("called `Option::unwrap()` on a `None` value");
                        Some(self.emit_enum_unwrap(enum_ptr, &inner_ty, SOME_TAG, msg))
                    }
                    "expect" => {
                        let msg = if let Some(arg) = m.args.first() {
                            self.emit_expr(&arg.expr)
                        } else {
                            self.emit_string_literal("called `Option::expect()` on a `None` value")
                        };
                        Some(self.emit_enum_unwrap(enum_ptr, &inner_ty, SOME_TAG, msg))
                    }
                    "unwrap_or" => {
                        let default_val = m
                            .args
                            .first()
                            .map(|a| self.emit_expr(&a.expr))
                            .unwrap_or_else(|| self.builder.ins().iconst(types::I64, 0));
                        Some(self.emit_enum_unwrap_or(enum_ptr, &inner_ty, SOME_TAG, default_val))
                    }
                    "map" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            let ret_ty = match &f_ty {
                                Type::Fn(_, ret) => *ret.clone(),
                                _ => Type::Void,
                            };
                            Some(self.emit_option_map(enum_ptr, &inner_ty, &ret_ty, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "and_then" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            Some(self.emit_option_and_then(enum_ptr, &inner_ty, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "or_else" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            Some(self.emit_option_or_else(enum_ptr, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    _ => None,
                }
            }
            Type::Generic(name, args) if name == "Result" => {
                let ok_ty = args.first().cloned().unwrap_or(Type::Void);
                let err_ty = args.get(1).cloned().unwrap_or(Type::Void);
                match m.method.as_str() {
                    "is_ok" => {
                        let tag =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), enum_ptr, 0i32);
                        let ok = self.builder.ins().iconst(types::I64, OK_TAG);
                        Some(self.builder.ins().icmp(IntCC::Equal, tag, ok))
                    }
                    "is_err" => {
                        let tag =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), enum_ptr, 0i32);
                        let err = self.builder.ins().iconst(types::I64, ERR_TAG);
                        Some(self.builder.ins().icmp(IntCC::Equal, tag, err))
                    }
                    "unwrap" => {
                        let msg =
                            self.emit_string_literal("called `Result::unwrap()` on an `Err` value");
                        Some(self.emit_enum_unwrap(enum_ptr, &ok_ty, OK_TAG, msg))
                    }
                    "expect" => {
                        let msg = if let Some(arg) = m.args.first() {
                            self.emit_expr(&arg.expr)
                        } else {
                            self.emit_string_literal("called `Result::expect()` on an `Err` value")
                        };
                        Some(self.emit_enum_unwrap(enum_ptr, &ok_ty, OK_TAG, msg))
                    }
                    "unwrap_or" => {
                        let default_val = m
                            .args
                            .first()
                            .map(|a| self.emit_expr(&a.expr))
                            .unwrap_or_else(|| self.builder.ins().iconst(types::I64, 0));
                        Some(self.emit_enum_unwrap_or(enum_ptr, &ok_ty, OK_TAG, default_val))
                    }
                    "unwrap_err" => {
                        let msg = self
                            .emit_string_literal("called `Result::unwrap_err()` on an `Ok` value");
                        Some(self.emit_enum_unwrap(enum_ptr, &err_ty, ERR_TAG, msg))
                    }
                    "map" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            let ret_ty = match &f_ty {
                                Type::Fn(_, ret) => *ret.clone(),
                                _ => Type::Void,
                            };
                            Some(
                                self.emit_result_map(
                                    enum_ptr, &ok_ty, &err_ty, &ret_ty, f_val, &f_ty,
                                ),
                            )
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "map_err" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            let ret_ty = match &f_ty {
                                Type::Fn(_, ret) => *ret.clone(),
                                _ => Type::Void,
                            };
                            Some(self.emit_result_map_err(
                                enum_ptr, &ok_ty, &err_ty, &ret_ty, f_val, &f_ty,
                            ))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "and_then" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            Some(self.emit_result_and_then(enum_ptr, &ok_ty, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "or_else" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            Some(self.emit_result_or_else(enum_ptr, &err_ty, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Emit: if enum tag == success_tag, return payload at offset 8; else panic(msg).
    fn emit_enum_unwrap(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        payload_ty: &Type,
        success_tag: i64,
        msg: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let expected = self.builder.ins().iconst(types::I64, success_tag);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, expected);

        let ok_block = self.builder.create_block();
        let fail_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], fail_block, &[]);

        self.builder.switch_to_block(fail_block);
        self.builder.seal_block(fail_block);
        let fid = self.func_ids["willow_panic"];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        self.builder.ins().call(fref, &[msg]);
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let clif_ty = clif_type(payload_ty);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        if clif_ty == types::F64 {
            self.builder.ins().bitcast(types::F64, MemFlags::new(), raw)
        } else if clif_ty == types::I8 {
            self.builder.ins().ireduce(types::I8, raw)
        } else {
            raw
        }
    }

    /// Emit: if enum tag == success_tag, return payload at offset 8; else return default.
    fn emit_enum_unwrap_or(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        payload_ty: &Type,
        success_tag: i64,
        default_val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let clif_ty = clif_type(payload_ty);
        let result_var = self.builder.declare_var(clif_ty);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let expected = self.builder.ins().iconst(types::I64, success_tag);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, expected);

        let ok_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge = self.builder.create_block();

        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], else_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = if clif_ty == types::F64 {
            self.builder.ins().bitcast(types::F64, MemFlags::new(), raw)
        } else if clif_ty == types::I8 {
            self.builder.ins().ireduce(types::I8, raw)
        } else {
            raw
        };
        self.builder.def_var(result_var, payload);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        self.builder.def_var(result_var, default_val);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit an indirect call through a function value.
    fn emit_indirect_call(
        &mut self,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
        args: &[cranelift_codegen::ir::Value],
    ) -> cranelift_codegen::ir::Value {
        if let Type::Fn(param_types, ret_type) = f_ty {
            let mut sig = self.module.make_signature();
            for pt in param_types {
                sig.params.push(AbiParam::new(clif_type(pt)));
            }
            let has_return = **ret_type != Type::Void;
            if has_return {
                sig.returns.push(AbiParam::new(clif_type(ret_type)));
            }
            let sig_ref = self.builder.import_signature(sig);
            let call = self.builder.ins().call_indirect(sig_ref, f_val, args);
            let results = self.builder.inst_results(call);
            if results.is_empty() {
                self.builder.ins().iconst(types::I64, 0)
            } else {
                results[0]
            }
        } else {
            self.builder.ins().iconst(types::I64, 0)
        }
    }

    /// Emit Option<T>.map(f) → Option<U>
    fn emit_option_map(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        inner_ty: &Type,
        ret_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let some_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_some = self.builder.ins().icmp(IntCC::Equal, tag, some_tag_val);

        let some_block = self.builder.create_block();
        let none_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_some, some_block, &[], none_block, &[]);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, inner_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        let new_some = self.emit_alloc_enum_variant(0, ret_ty, result);
        self.builder.def_var(result_var, new_some);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        let new_none = self.emit_alloc_none();
        self.builder.def_var(result_var, new_none);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Option<T>.and_then(f) where f: fn(T) -> Option<U>
    fn emit_option_and_then(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        inner_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let some_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_some = self.builder.ins().icmp(IntCC::Equal, tag, some_tag_val);

        let some_block = self.builder.create_block();
        let none_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_some, some_block, &[], none_block, &[]);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, inner_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        self.builder.def_var(result_var, result);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        let new_none = self.emit_alloc_none();
        self.builder.def_var(result_var, new_none);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Option<T>.or_else(f) where f: fn() -> Option<T>
    fn emit_option_or_else(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let some_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_some = self.builder.ins().icmp(IntCC::Equal, tag, some_tag_val);

        let some_block = self.builder.create_block();
        let none_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_some, some_block, &[], none_block, &[]);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        self.builder.def_var(result_var, ptr);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        let result = self.emit_indirect_call(f_val, f_ty, &[]);
        self.builder.def_var(result_var, result);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Result<T,E>.map(f) where f: fn(T) -> U → Result<U, E>
    fn emit_result_map(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        ok_ty: &Type,
        err_ty: &Type,
        ret_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let ok_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag_val);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, ok_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        let new_ok = self.emit_alloc_enum_variant(0, ret_ty, result);
        self.builder.def_var(result_var, new_ok);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        let err_raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let new_err = self.emit_alloc_enum_variant_raw(1, err_ty, err_raw);
        self.builder.def_var(result_var, new_err);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Result<T,E>.map_err(f) where f: fn(E) -> F → Result<T, F>
    fn emit_result_map_err(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        ok_ty: &Type,
        err_ty: &Type,
        ret_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let ok_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag_val);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let ok_raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let new_ok = self.emit_alloc_enum_variant_raw(0, ok_ty, ok_raw);
        self.builder.def_var(result_var, new_ok);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, err_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        let new_err = self.emit_alloc_enum_variant(1, ret_ty, result);
        self.builder.def_var(result_var, new_err);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Result<T,E>.and_then(f) where f: fn(T) -> Result<U,E>
    fn emit_result_and_then(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        ok_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let ok_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag_val);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, ok_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        self.builder.def_var(result_var, result);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        self.builder.def_var(result_var, ptr);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Result<T,E>.or_else(f) where f: fn(E) -> Result<T,F>
    fn emit_result_or_else(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        err_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let ok_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag_val);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        self.builder.def_var(result_var, ptr);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, err_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        self.builder.def_var(result_var, result);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Allocate a new 2-word enum (tag + payload) where payload is a typed value.
    fn emit_alloc_enum_variant(
        &mut self,
        tag: i64,
        payload_ty: &Type,
        payload_val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let payload_is_gc = is_gc_managed(payload_ty, self.enum_infos);
        let gc_mask: i64 = if payload_is_gc { 0b10 } else { 0 };
        // Root the payload across the enum allocation: a GC-managed payload is a
        // live pointer that must survive the collection `willow_alloc_typed` may
        // trigger before we store it into the new enum. Only reference payloads
        // are rooted; rooting a scalar word would make the GC mark it as a
        // bogus object pointer.
        if payload_is_gc {
            self.emit_push_root(payload_val);
        }
        let size = self.builder.ins().iconst(types::I64, 16);
        let mask = self.builder.ins().iconst(types::I64, gc_mask);
        let alloc_id = self.func_ids["willow_alloc_typed"];
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let ptr = self.builder.inst_results(call)[0];
        let tag_val = self.builder.ins().iconst(types::I64, tag);
        self.builder
            .ins()
            .store(MemFlags::new(), tag_val, ptr, 0i32);
        let payload_i64 = if matches!(payload_ty, Type::F64) {
            self.builder
                .ins()
                .bitcast(types::I64, MemFlags::new(), payload_val)
        } else if matches!(payload_ty, Type::Bool) {
            self.builder.ins().uextend(types::I64, payload_val)
        } else {
            payload_val
        };
        self.builder
            .ins()
            .store(MemFlags::new(), payload_i64, ptr, 8i32);
        if payload_is_gc {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        ptr
    }

    /// Allocate a new 2-word enum (tag + payload) where payload is already an i64 raw word.
    fn emit_alloc_enum_variant_raw(
        &mut self,
        tag: i64,
        payload_ty: &Type,
        payload_raw: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let payload_is_gc = is_gc_managed(payload_ty, self.enum_infos);
        let gc_mask: i64 = if payload_is_gc { 0b10 } else { 0 };
        // See `emit_alloc_enum_variant`: root a GC-managed payload across the
        // allocation so a collection cannot free it before it is stored.
        if payload_is_gc {
            self.emit_push_root(payload_raw);
        }
        let size = self.builder.ins().iconst(types::I64, 16);
        let mask = self.builder.ins().iconst(types::I64, gc_mask);
        let alloc_id = self.func_ids["willow_alloc_typed"];
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let ptr = self.builder.inst_results(call)[0];
        let tag_val = self.builder.ins().iconst(types::I64, tag);
        self.builder
            .ins()
            .store(MemFlags::new(), tag_val, ptr, 0i32);
        self.builder
            .ins()
            .store(MemFlags::new(), payload_raw, ptr, 8i32);
        if payload_is_gc {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        ptr
    }

    /// Allocate an Option::None (1-word enum, tag=1, no payload).
    fn emit_alloc_none(&mut self) -> cranelift_codegen::ir::Value {
        let size = self.builder.ins().iconst(types::I64, 8);
        let mask = self.builder.ins().iconst(types::I64, 0);
        let alloc_id = self.func_ids["willow_alloc_typed"];
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let ptr = self.builder.inst_results(call)[0];
        let none_tag = self.builder.ins().iconst(types::I64, 1);
        self.builder
            .ins()
            .store(MemFlags::new(), none_tag, ptr, 0i32);
        ptr
    }

    /// Convert a raw i64 word back to the appropriate CLIF value for the given type.
    fn coerce_i64_to(
        &mut self,
        raw: cranelift_codegen::ir::Value,
        ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        match ty {
            Type::F64 => self.builder.ins().bitcast(types::F64, MemFlags::new(), raw),
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
            Type::F64 => self.builder.ins().bitcast(types::I64, MemFlags::new(), val),
            Type::Bool => self.builder.ins().uextend(types::I64, val),
            _ => val,
        }
    }

    /// Emit `[e0, e1, ...]`: allocate an array sized to the literal, then store
    /// each element through the array ABI. The array is rooted during element
    /// evaluation so a GC triggered mid-construction keeps it (and its stored
    /// elements) alive.
    fn emit_array_literal(
        &mut self,
        elements: &[Expr],
        elem_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let len_val = self.builder.ins().iconst(types::I64, elements.len() as i64);
        let is_ref = if is_gc_managed(elem_ty, self.enum_infos) {
            1
        } else {
            0
        };
        let is_ref_val = self.builder.ins().iconst(types::I64, is_ref);
        let new_id = self.func_ids["willow_array_new"];
        let new_ref = self.module.declare_func_in_func(new_id, self.builder.func);
        let call = self.builder.ins().call(new_ref, &[len_val, is_ref_val]);
        let arr = self.builder.inst_results(call)[0];

        self.emit_push_root(arr);
        for (i, el) in elements.iter().enumerate() {
            // Box class elements when the array's element type is an interface.
            let val = self.emit_expr_coerced(el, elem_ty);
            let word = self.coerce_to_i64(val, elem_ty);
            let idx_val = self.builder.ins().iconst(types::I64, i as i64);
            let set_id = self.func_ids["willow_array_set"];
            let set_ref = self.module.declare_func_in_func(set_id, self.builder.func);
            self.builder.ins().call(set_ref, &[arr, idx_val, word]);
        }
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        arr
    }

    /// Emit `arr[index]`: bounds-checked element read, converted back to the
    /// element type.
    fn emit_index(
        &mut self,
        arr_expr: &Expr,
        index_expr: &Expr,
        elem_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let arr = self.emit_expr(arr_expr);
        let index = self.emit_expr(index_expr);
        let get_id = self.func_ids["willow_array_get"];
        let get_ref = self.module.declare_func_in_func(get_id, self.builder.func);
        let call = self.builder.ins().call(get_ref, &[arr, index]);
        let word = self.builder.inst_results(call)[0];
        self.coerce_i64_to(word, elem_ty)
    }

    /// Emit a `Map<K, V>` method call. Keys/values cross the runtime ABI as raw
    /// 64-bit words plus ref-ness flags; `get` returns a runtime-built
    /// `Option<V>` pointer.
    fn emit_map_method_call(
        &mut self,
        map: cranelift_codegen::ir::Value,
        key_ty: &Type,
        val_ty: &Type,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        let key_is_ref = self.builder.ins().iconst(
            types::I64,
            i64::from(is_gc_managed(key_ty, self.enum_infos)),
        );
        match m.method.as_str() {
            "insert" => {
                // Root the map while evaluating key/value (either may allocate).
                self.emit_push_root(map);
                let k = self.emit_expr(&m.args[0].expr);
                let k_word = self.coerce_to_i64(k, key_ty);
                let v = self.emit_expr(&m.args[1].expr);
                let v_word = self.coerce_to_i64(v, val_ty);
                let val_is_ref = self.builder.ins().iconst(
                    types::I64,
                    i64::from(is_gc_managed(val_ty, self.enum_infos)),
                );
                let id = self.func_ids["willow_map_insert"];
                let r = self.module.declare_func_in_func(id, self.builder.func);
                self.builder
                    .ins()
                    .call(r, &[map, k_word, key_is_ref, v_word, val_is_ref]);
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
                self.builder.ins().iconst(types::I64, 0) // void
            }
            "get" => {
                let k = self.emit_expr(&m.args[0].expr);
                let k_word = self.coerce_to_i64(k, key_ty);
                let id = self.func_ids["willow_map_get"];
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[map, k_word, key_is_ref]);
                self.builder.inst_results(call)[0] // Option<V> pointer
            }
            "contains" => {
                let k = self.emit_expr(&m.args[0].expr);
                let k_word = self.coerce_to_i64(k, key_ty);
                let id = self.func_ids["willow_map_contains"];
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[map, k_word, key_is_ref]);
                let raw = self.builder.inst_results(call)[0];
                self.builder.ins().ireduce(types::I8, raw) // bool
            }
            "len" => {
                let id = self.func_ids["willow_map_len"];
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[map]);
                self.builder.inst_results(call)[0]
            }
            _ => self.builder.ins().iconst(types::I64, 0),
        }
    }

    /// Dispatch a method call through an interface box: load the concrete object
    /// (word 0) and vtable (word 1), load the slot's function pointer, and make an
    /// indirect call with the object as the first argument (spec §8.3 / §9.4).
    fn emit_interface_dispatch(
        &mut self,
        box_ptr: cranelift_codegen::ir::Value,
        iface: &InterfaceInfo,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        let Some(slot) = iface.method_order.iter().position(|n| n == &m.method) else {
            // Not an interface method — already rejected by the type checker (E0418).
            return self.builder.ins().iconst(types::I64, 0);
        };
        let method = iface.methods[&m.method].clone();
        let ret_type = method.return_type.clone();
        let param_types = method.params.clone();

        // Load object (word 0, GC ref) and vtable (word 1, raw) from the box.
        let obj = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), box_ptr, 0i32);
        let vtable = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), box_ptr, 8i32);
        let fnptr = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), vtable, (slot * 8) as i32);

        // Root the concrete object across argument evaluation (args may allocate).
        self.emit_push_root(obj);
        let (arg_vals, temp_roots) = self.emit_call_args_rooted_coerced(
            Some(&m.method),
            None,
            None,
            Some(&param_types),
            &m.args,
        );

        // Indirect-call signature: (object ptr, params...) -> ret.
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        for pt in &param_types {
            sig.params.push(AbiParam::new(clif_type(pt)));
        }
        if ret_type != Type::Void {
            sig.returns.push(AbiParam::new(clif_type(&ret_type)));
        }
        let sig_ref = self.builder.import_signature(sig);

        let mut call_args = vec![obj];
        call_args.extend(arg_vals);
        let call = self.builder.ins().call_indirect(sig_ref, fnptr, &call_args);
        let result = if ret_type != Type::Void {
            self.builder.inst_results(call)[0]
        } else {
            self.builder.ins().iconst(types::I64, 0)
        };

        // Pop arg roots + the object root.
        self.emit_pop_roots_n(temp_roots + 1);
        self.gc_root_count -= temp_roots + 1;
        result
    }

    fn emit_method_call(&mut self, m: &MethodCallExpr) -> cranelift_codegen::ir::Value {
        let self_ptr = self.emit_expr(&m.object);
        let obj_type = self.ast_type_of(&m.object);

        // Built-in primitive `toString()` (willow-fvfc): i64/f64/bool convert via
        // a runtime call; String is identity (no allocation).
        if m.method == "toString" && m.args.is_empty() {
            match obj_type {
                Type::String => return self_ptr,
                Type::I64 | Type::F64 | Type::Bool => {
                    let runtime = match obj_type {
                        Type::I64 => "willow_i64_to_string",
                        Type::F64 => "willow_f64_to_string",
                        _ => "willow_bool_to_string",
                    };
                    let fid = self.func_ids[runtime];
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    let call = self.builder.ins().call(fref, &[self_ptr]);
                    return self.builder.inst_results(call)[0];
                }
                _ => {}
            }
        }

        if let Some(val) = self.emit_option_result_method_call(self_ptr, &obj_type.clone(), m) {
            return val;
        }

        if m.method == "join" {
            if let Some(result_ty) = join_handle_result_type(&obj_type) {
                // Drive the cooperative scheduler until the spawned task (and any
                // others it depends on) completes, then read the result from the
                // frame's slot 0. `self_ptr` is the spawn frame (the JoinHandle).
                self.emit_push_root(self_ptr);
                let run_fid = self.func_ids["willow_sched_run"];
                let run_fref = self.module.declare_func_in_func(run_fid, self.builder.func);
                self.builder.ins().call(run_fref, &[]);

                if result_ty == Type::Void {
                    self.emit_pop_roots_n(1);
                    self.gc_root_count -= 1;
                    return self.builder.ins().iconst(types::I8, 0);
                }
                let clif_ret_ty = clif_type(&result_ty);
                let result_off = async_frame_slot_offset(0);
                let result =
                    self.builder
                        .ins()
                        .load(clif_ret_ty, MemFlags::new(), self_ptr, result_off);
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
                return result;
            }
        }

        if let Some(element_ty) = channel_element_type(&obj_type) {
            return self.emit_channel_method_call(self_ptr, &element_ty, m);
        }

        // Array `.len()` → willow_array_len(arr).
        if let Type::Array(elem_ty) = &obj_type {
            let elem_ty = (**elem_ty).clone();
            match m.method.as_str() {
                "len" => {
                    let id = self.func_ids["willow_array_len"];
                    let r = self.module.declare_func_in_func(id, self.builder.func);
                    let call = self.builder.ins().call(r, &[self_ptr]);
                    return self.builder.inst_results(call)[0];
                }
                "push" => {
                    // Root the array while the value is evaluated (it may allocate).
                    self.emit_push_root(self_ptr);
                    // Box a class argument when the element type is an interface.
                    let v = self.emit_expr_coerced(&m.args[0].expr, &elem_ty);
                    let word = self.coerce_to_i64(v, &elem_ty);
                    let id = self.func_ids["willow_array_push"];
                    let r = self.module.declare_func_in_func(id, self.builder.func);
                    self.builder.ins().call(r, &[self_ptr, word]);
                    self.emit_pop_roots_n(1);
                    self.gc_root_count -= 1;
                    return self.builder.ins().iconst(types::I8, 0); // void
                }
                "pop" => {
                    let id = self.func_ids["willow_array_pop"];
                    let r = self.module.declare_func_in_func(id, self.builder.func);
                    let call = self.builder.ins().call(r, &[self_ptr]);
                    let word = self.builder.inst_results(call)[0];
                    return self.coerce_i64_to(word, &elem_ty);
                }
                _ => {}
            }
        }

        // Map<K,V> methods.
        if let Type::Generic(name, margs) = &obj_type {
            if name == "Map" && margs.len() == 2 {
                let key_ty = margs[0].clone();
                let val_ty = margs[1].clone();
                return self.emit_map_method_call(self_ptr, &key_ty, &val_ty, m);
            }
        }

        // Debug build: guard against nil dereference with a source-aware runtime error.
        if self.build_mode == BuildMode::Debug {
            let span = m.object.span();
            self.emit_nil_check(self_ptr, span, &m.method.clone());
        }

        // Interface dispatch: the receiver is an interface box {object, vtable}.
        // Must be checked before class dispatch, since an interface is also a
        // `Type::Named` that `class_name_for_object_type` would accept. A generic
        // interface instantiation (`Box<String>`) dispatches identically — the
        // vtable is keyed by the interface name (willow-1js.1).
        if let Type::Generic(name, _) = &obj_type {
            if let Some(iface) = self.interface_infos.get(name).cloned() {
                let pushed = self.emit_callstack_push(&m.method, m.span);
                let r = self.emit_interface_dispatch(self_ptr, &iface, m);
                if pushed {
                    self.emit_callstack_pop();
                }
                return r;
            }
        }
        if let Some(iface_name) = class_name_for_object_type(&obj_type) {
            if let Some(iface) = self.interface_infos.get(&iface_name).cloned() {
                let pushed = self.emit_callstack_push(&m.method, m.span);
                let r = self.emit_interface_dispatch(self_ptr, &iface, m);
                if pushed {
                    self.emit_callstack_pop();
                }
                return r;
            }
        }

        if let Some(class_name) = class_name_for_object_type(&obj_type) {
            let method_name = m.method.clone();

            // Build the dispatch list keyed by runtime type_id. For each class,
            // resolve the method to the nearest class in its hierarchy (itself
            // then ancestors) that defines it, so a subclass that INHERITS the
            // method (no override) still dispatches to the inherited
            // implementation instead of falling through (willow-ftk).
            let mut dispatch_list: Vec<(i64, String)> = self
                .class_type_ids
                .iter()
                .filter_map(|(cls, &id)| {
                    let mut search = Some(cls.clone());
                    let mut seen = HashSet::new();
                    while let Some(name) = search {
                        if !seen.insert(name.clone()) {
                            break;
                        }
                        let mangled =
                            class_method_symbol_name(self.known_modules, &name, &method_name);
                        if self.func_ids.contains_key(&mangled) {
                            return Some((id, name));
                        }
                        search = self.class_base.get(&name).cloned();
                    }
                    None
                })
                .collect();
            dispatch_list.sort_by_key(|(id, _)| *id);

            if dispatch_list.is_empty() {
                return self.builder.ins().iconst(types::I64, 0);
            }

            // Debug call-chain frame for the method invocation (willow-phx3).
            // Pushed once in the entry block; popped before the single-dispatch
            // return and in the dynamic-dispatch merge block (a panicking method
            // never reaches the pop, leaving its frame on the chain).
            let method_frame_pushed = self.emit_callstack_push(&method_name, m.span);

            // Determine the return type from the static class's method (or first found).
            let base_mangled =
                class_method_symbol_name(self.known_modules, &class_name, &method_name);
            let ret_type = self
                .func_return_types
                .get(&base_mangled)
                .cloned()
                .or_else(|| {
                    dispatch_list.first().and_then(|(_, cls)| {
                        let mn = class_method_symbol_name(self.known_modules, cls, &method_name);
                        self.func_return_types.get(&mn).cloned()
                    })
                })
                .unwrap_or(Type::Void);

            if dispatch_list.len() == 1 {
                // Fast path: only one implementation, no need for a dispatch chain.
                let (_, cls) = &dispatch_list[0];
                let mangled = class_method_symbol_name(self.known_modules, cls, &method_name);
                let &func_id = self.func_ids.get(&mangled).unwrap();
                let func_ref = self.module.declare_func_in_func(func_id, self.builder.func);
                let modes = self.func_param_modes.get(&mangled).cloned();
                let param_debug = self.func_param_debug.get(&mangled).cloned();
                let param_types = self.method_param_types(&mangled);
                let has_reference_args = has_reference_args(modes.as_deref(), &m.args);
                let user_callee = format!("{cls}::{method_name}");
                let (arg_vals, temp_roots) = self.emit_call_args_rooted_coerced(
                    Some(&user_callee),
                    modes.as_deref(),
                    param_debug.as_deref(),
                    param_types.as_deref(),
                    &m.args,
                );
                let mut call_args = vec![self_ptr];
                call_args.extend(arg_vals);
                let call = self.builder.ins().call(func_ref, &call_args);
                let result = if ret_type != Type::Void {
                    self.builder.inst_results(call)[0]
                } else {
                    self.builder.ins().iconst(types::I64, 0)
                };
                if has_reference_args {
                    self.emit_debug_reference_call_clear();
                }
                if method_frame_pushed {
                    self.emit_callstack_pop();
                }
                self.emit_pop_roots_n(temp_roots);
                self.gc_root_count -= temp_roots;
                return result;
            }

            // Dynamic dispatch: load runtime type_id from word 0 of the object.
            let runtime_type_id =
                self.builder
                    .ins()
                    .load(types::I64, MemFlags::new(), self_ptr, 0i32);

            // Use an SSA variable to collect the result across dispatch arms
            // (matches the pattern used by emit_short_circuit_and/or and emit_match).
            let ret_clif_ty = clif_type(&ret_type);
            let result_var = if ret_type != Type::Void {
                let v = self.builder.declare_var(ret_clif_ty);
                let zero = if ret_clif_ty == types::F64 {
                    let bits = self.builder.ins().iconst(types::I64, 0);
                    self.builder
                        .ins()
                        .bitcast(types::F64, MemFlags::new(), bits)
                } else if ret_clif_ty == types::I8 {
                    self.builder.ins().iconst(types::I8, 0)
                } else {
                    self.builder.ins().iconst(types::I64, 0)
                };
                self.builder.def_var(v, zero);
                Some(v)
            } else {
                None
            };

            let merge_block = self.builder.create_block();

            let dispatch_len = dispatch_list.len();
            for (i, (type_id, cls)) in dispatch_list.iter().enumerate() {
                let mangled = class_method_symbol_name(self.known_modules, cls, &method_name);
                let &func_id = match self.func_ids.get(&mangled) {
                    Some(id) => id,
                    None => continue,
                };

                let type_id_const = self.builder.ins().iconst(types::I64, *type_id);
                let is_match =
                    self.builder
                        .ins()
                        .icmp(IntCC::Equal, runtime_type_id, type_id_const);

                let match_block = self.builder.create_block();
                let next_block = self.builder.create_block();
                self.builder
                    .ins()
                    .brif(is_match, match_block, &[], next_block, &[]);

                // --- match arm ---
                self.builder.switch_to_block(match_block);
                self.builder.seal_block(match_block);
                let func_ref = self.module.declare_func_in_func(func_id, self.builder.func);
                let modes = self.func_param_modes.get(&mangled).cloned();
                let param_debug = self.func_param_debug.get(&mangled).cloned();
                let param_types = self.method_param_types(&mangled);
                let has_reference_args = has_reference_args(modes.as_deref(), &m.args);
                let user_callee = format!("{cls}::{method_name}");
                let (arg_vals, temp_roots) = self.emit_call_args_rooted_coerced(
                    Some(&user_callee),
                    modes.as_deref(),
                    param_debug.as_deref(),
                    param_types.as_deref(),
                    &m.args,
                );
                let mut call_args = vec![self_ptr];
                call_args.extend(arg_vals);
                let call = self.builder.ins().call(func_ref, &call_args);
                if let Some(rv) = result_var {
                    let result = self.builder.inst_results(call)[0];
                    self.builder.def_var(rv, result);
                }
                if has_reference_args {
                    self.emit_debug_reference_call_clear();
                }
                self.emit_pop_roots_n(temp_roots);
                self.gc_root_count -= temp_roots;
                self.builder.ins().jump(merge_block, &[]);

                // --- no-match: continue to next candidate ---
                self.builder.switch_to_block(next_block);
                self.builder.seal_block(next_block);

                // On the last candidate, fall through to merge with the default (zero) result.
                if i + 1 == dispatch_len {
                    self.builder.ins().jump(merge_block, &[]);
                }
            }

            self.builder.switch_to_block(merge_block);
            self.builder.seal_block(merge_block);
            // Pop the method call-chain frame on the normal-return path (a
            // panicking arm jumps to abort and never reaches here) (willow-phx3).
            if method_frame_pushed {
                self.emit_callstack_pop();
            }
            if let Some(rv) = result_var {
                return self.builder.use_var(rv);
            }
            return self.builder.ins().iconst(types::I64, 0);
        }
        self.builder.ins().iconst(types::I64, 0)
    }

    fn emit_channel_method_call(
        &mut self,
        channel_ptr: cranelift_codegen::ir::Value,
        element_ty: &Type,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        match m.method.as_str() {
            "send" => {
                if let Some(arg) = m.args.first() {
                    let runtime_name =
                        format!("willow_channel_send_{}", channel_runtime_suffix(element_ty));
                    let fid = self.func_ids[&runtime_name];
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    let value = self.emit_expr(&arg.expr);
                    self.builder.ins().call(fref, &[channel_ptr, value]);
                }
                self.builder.ins().iconst(types::I8, 0)
            }
            "recv" => {
                let runtime_name =
                    format!("willow_channel_recv_{}", channel_runtime_suffix(element_ty));
                let fid = self.func_ids[&runtime_name];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                let call = self.builder.ins().call(fref, &[channel_ptr]);
                self.builder.inst_results(call)[0]
            }
            "close" => {
                let fid = self.func_ids["willow_channel_close"];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[channel_ptr]);
                self.builder.ins().iconst(types::I8, 0)
            }
            _ => self.builder.ins().iconst(types::I64, 0),
        }
    }

    /// Eager (block-driving) `select` (willow-7aj): probe each case in source
    /// order — a recv case is ready when its channel has a value or is closed; a
    /// send case (unbounded channel) is always ready. The first ready case runs.
    /// If none is ready and there is a `default`, it runs; otherwise the scheduler
    /// is driven and the probe retried (giving up if no task could progress). In a
    /// non-task context recv_ready does not register a waiter (current task is 0),
    /// so it is a pure readiness probe here.
    fn emit_select(&mut self, s: &SelectExpr) {
        let loop_b = self.builder.create_block();
        let done_b = self.builder.create_block();
        let case_blocks: Vec<_> = s
            .cases
            .iter()
            .map(|_| self.builder.create_block())
            .collect();
        let mut cont_blocks = Vec::new();
        let default_idx = s
            .cases
            .iter()
            .position(|c| matches!(c.kind, SelectCaseKind::Default));

        self.builder.ins().jump(loop_b, &[]);
        self.builder.switch_to_block(loop_b);
        let mut chain_ended = false;
        for (i, case) in s.cases.iter().enumerate() {
            match &case.kind {
                SelectCaseKind::Recv { channel, .. } => {
                    let ch = self.emit_expr(channel);
                    let ready_fid = self.func_ids["willow_channel_recv_ready"];
                    let ready_ref = self
                        .module
                        .declare_func_in_func(ready_fid, self.builder.func);
                    let rcall = self.builder.ins().call(ready_ref, &[ch]);
                    let ready = self.builder.inst_results(rcall)[0];
                    let cont = self.builder.create_block();
                    cont_blocks.push(cont);
                    let zero = self.builder.ins().iconst(types::I32, 0);
                    let is_ready = self.builder.ins().icmp(IntCC::NotEqual, ready, zero);
                    self.builder
                        .ins()
                        .brif(is_ready, case_blocks[i], &[], cont, &[]);
                    self.builder.switch_to_block(cont);
                }
                SelectCaseKind::Send { .. } => {
                    self.builder.ins().jump(case_blocks[i], &[]);
                    chain_ended = true;
                    break;
                }
                SelectCaseKind::Default => {}
            }
        }
        if !chain_ended {
            if let Some(di) = default_idx {
                self.builder.ins().jump(case_blocks[di], &[]);
            } else {
                let run_fid = self.func_ids["willow_sched_run"];
                let run_ref = self.module.declare_func_in_func(run_fid, self.builder.func);
                let rcall = self.builder.ins().call(run_ref, &[]);
                let completed = self.builder.inst_results(rcall)[0];
                let zero = self.builder.ins().iconst(types::I64, 0);
                let progressed = self.builder.ins().icmp(IntCC::NotEqual, completed, zero);
                self.builder
                    .ins()
                    .brif(progressed, loop_b, &[], done_b, &[]);
            }
        }

        for (i, case) in s.cases.iter().enumerate() {
            self.builder.switch_to_block(case_blocks[i]);
            let saved_vars = self.vars.clone();
            let saved_roots = self.gc_root_count;
            self.terminated = false;
            match &case.kind {
                SelectCaseKind::Recv { binding, channel } => {
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(channel)).unwrap_or(Type::I64);
                    let ch = self.emit_expr(channel);
                    let recv_name =
                        format!("willow_channel_recv_{}", channel_runtime_suffix(&elem_ty));
                    let recv_fid = self.func_ids[&recv_name];
                    let recv_ref = self
                        .module
                        .declare_func_in_func(recv_fid, self.builder.func);
                    let vcall = self.builder.ins().call(recv_ref, &[ch]);
                    let v = self.builder.inst_results(vcall)[0];
                    if binding != "_" {
                        let storage = self.create_local_stack_slot(&elem_ty, v);
                        self.vars.insert(binding.clone(), storage);
                    }
                    self.emit_block(&case.body);
                }
                SelectCaseKind::Send { channel, value } => {
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(channel)).unwrap_or(Type::I64);
                    let ch = self.emit_expr(channel);
                    let val = self.emit_expr(value);
                    let send_name =
                        format!("willow_channel_send_{}", channel_runtime_suffix(&elem_ty));
                    let send_fid = self.func_ids[&send_name];
                    let send_ref = self
                        .module
                        .declare_func_in_func(send_fid, self.builder.func);
                    self.builder.ins().call(send_ref, &[ch, val]);
                    self.emit_block(&case.body);
                }
                SelectCaseKind::Default => self.emit_block(&case.body),
            }
            if !self.terminated {
                self.builder.ins().jump(done_b, &[]);
            }
            self.vars = saved_vars;
            self.gc_root_count = saved_roots;
        }

        self.builder.seal_block(loop_b);
        for c in &cont_blocks {
            self.builder.seal_block(*c);
        }
        for c in &case_blocks {
            self.builder.seal_block(*c);
        }
        self.builder.seal_block(done_b);
        self.builder.switch_to_block(done_b);
        self.terminated = false;
    }

    fn emit_spawn(&mut self, s: &SpawnExpr) -> cranelift_codegen::ir::Value {
        // Spawning a cooperative-leaf async fn is exactly calling its constructor:
        // that already allocates the leaf frame, stores the args, and schedules
        // the leaf's poll task on the cooperative scheduler. The leaf frame IS the
        // JoinHandle (its slot 0 is the result), so `join` reads the leaf's real
        // result rather than a wrapper frame's stored frame-ptr.
        if self.cooperative_leaves.contains(&s.callee) {
            if let Some(&ctor_fid) = self.func_ids.get(&s.callee) {
                let modes = self.func_param_modes.get(&s.callee).cloned();
                let (arg_vals, arg_roots) =
                    self.emit_call_args_rooted(Some(&s.callee), modes.as_deref(), None, &s.args);
                let ctor_ref = self
                    .module
                    .declare_func_in_func(ctor_fid, self.builder.func);
                let call = self.builder.ins().call(ctor_ref, &arg_vals);
                let result = self.builder.inst_results(call)[0];
                if arg_roots > 0 {
                    self.emit_pop_roots_n(arg_roots);
                    self.gc_root_count -= arg_roots;
                }
                return result;
            }
        }

        // Cooperative spawn for named functions with a pre-compiled poll fn: the
        // task runs on the single-threaded scheduler (willow_sched_spawn), not an
        // OS thread. The frame is a GC async-frame: slot 0 = result, slots 1.. =
        // args; `join` drives the scheduler and reads slot 0.
        if let Some(tramp_name) = self
            .spawn_tramp_names
            .get(&s.span)
            .filter(|_| !self.spawn_fptr_sigs.contains_key(&s.span))
            .cloned()
        {
            if let Some(&poll_fid) = self.func_ids.get(&tramp_name) {
                let ret_ty = self
                    .func_return_types
                    .get(&s.callee)
                    .cloned()
                    .unwrap_or(Type::Void);
                let param_types: Vec<Type> = match self.fn_types.get(&s.callee) {
                    Some(Type::Fn(p, _)) => p.clone(),
                    _ => s.args.iter().map(|_| Type::I64).collect(),
                };

                // Frame layout: slot 0 = result, slots 1.. = args. The GC mask
                // marks the result/arg slots that hold heap references.
                let slot_count = (1 + s.args.len()) as i64;
                let mut mask = 0i64;
                if is_gc_managed(&ret_ty, self.enum_infos) {
                    mask |= 1;
                }
                for (i, pt) in param_types.iter().enumerate() {
                    if is_gc_managed(pt, self.enum_infos) {
                        mask |= 1 << (1 + i);
                    }
                }

                // Compute the args first, then root any GC-managed ones across the
                // frame allocation so a collection cannot free them before they are
                // stored into the (not-yet-rooted) frame.
                let modes = self.func_param_modes.get(&s.callee).cloned();
                let (arg_vals, arg_roots) =
                    self.emit_call_args_rooted(Some(&s.callee), modes.as_deref(), None, &s.args);

                // frame = willow_async_frame_alloc(slot_count, mask)
                let alloc_fid = self.func_ids["willow_async_frame_alloc"];
                let alloc_fref = self
                    .module
                    .declare_func_in_func(alloc_fid, self.builder.func);
                let sc = self.builder.ins().iconst(types::I64, slot_count);
                let mk = self.builder.ins().iconst(types::I64, mask);
                let alloc_call = self.builder.ins().call(alloc_fref, &[sc, mk]);
                let frame = self.builder.inst_results(alloc_call)[0];

                // Store each arg into its frame slot (slot 1..).
                for (i, val) in arg_vals.iter().enumerate() {
                    let offset = async_frame_slot_offset(1 + i);
                    self.builder
                        .ins()
                        .store(MemFlags::new(), *val, frame, offset);
                }

                // willow_sched_spawn(poll_addr, frame) — GC-roots the frame.
                let poll_fref = self
                    .module
                    .declare_func_in_func(poll_fid, self.builder.func);
                let poll_addr = self.builder.ins().func_addr(types::I64, poll_fref);
                let spawn_fid = self.func_ids["willow_sched_spawn"];
                let spawn_fref = self
                    .module
                    .declare_func_in_func(spawn_fid, self.builder.func);
                self.builder.ins().call(spawn_fref, &[poll_addr, frame]);

                // The frame now owns the GC args; drop their temporary roots.
                if arg_roots > 0 {
                    self.emit_pop_roots_n(arg_roots);
                    self.gc_root_count -= arg_roots;
                }

                return frame;
            }
        }

        // Function-pointer spawn: the callee is a `Fn` value in a local, so it
        // gets a `call_indirect` poll trampoline (compiled in compile_all). Like
        // named spawns, the task is scheduled on the cooperative scheduler instead
        // of running inline (willow-spawn-fptr).
        //
        // Frame layout: slot 0 = result, slot 1 = function pointer, slots 2.. =
        // args. The fptr is a raw code address (never a GC reference), so only the
        // result/arg slots are marked in the GC mask.
        if let Some(tramp_name) = self.spawn_tramp_names.get(&s.span).cloned() {
            if let (Some(&poll_fid), Some(storage)) = (
                self.func_ids.get(&tramp_name),
                self.vars.get(&s.callee).cloned(),
            ) {
                if let Type::Fn(param_types, ret_type) = storage.ty().clone() {
                    let slot_count = (2 + s.args.len()) as i64;
                    let mut mask = 0i64;
                    if is_gc_managed(&ret_type, self.enum_infos) {
                        mask |= 1;
                    }
                    for (i, pt) in param_types.iter().enumerate() {
                        if is_gc_managed(pt, self.enum_infos) {
                            mask |= 1 << (2 + i);
                        }
                    }

                    // Load the function pointer, then compute the args. Root the
                    // fptr value? No — a code address is not a heap object, so it
                    // needs no GC root; GC-managed args are rooted across the frame
                    // allocation by emit_call_args_rooted_coerced.
                    let callee_val = self.load_var(&storage);
                    let (arg_vals, arg_roots) = self.emit_call_args_rooted_coerced(
                        Some(&s.callee),
                        None,
                        None,
                        Some(&param_types),
                        &s.args,
                    );

                    // frame = willow_async_frame_alloc(slot_count, mask)
                    let alloc_fid = self.func_ids["willow_async_frame_alloc"];
                    let alloc_fref = self
                        .module
                        .declare_func_in_func(alloc_fid, self.builder.func);
                    let sc = self.builder.ins().iconst(types::I64, slot_count);
                    let mk = self.builder.ins().iconst(types::I64, mask);
                    let alloc_call = self.builder.ins().call(alloc_fref, &[sc, mk]);
                    let frame = self.builder.inst_results(alloc_call)[0];

                    // Store the fptr into slot 1 and the args into slots 2..
                    self.builder.ins().store(
                        MemFlags::new(),
                        callee_val,
                        frame,
                        async_frame_slot_offset(1),
                    );
                    for (i, val) in arg_vals.iter().enumerate() {
                        let offset = async_frame_slot_offset(2 + i);
                        self.builder
                            .ins()
                            .store(MemFlags::new(), *val, frame, offset);
                    }

                    // willow_sched_spawn(poll_addr, frame) — GC-roots the frame.
                    let poll_fref = self
                        .module
                        .declare_func_in_func(poll_fid, self.builder.func);
                    let poll_addr = self.builder.ins().func_addr(types::I64, poll_fref);
                    let spawn_fid = self.func_ids["willow_sched_spawn"];
                    let spawn_fref = self
                        .module
                        .declare_func_in_func(spawn_fid, self.builder.func);
                    self.builder.ins().call(spawn_fref, &[poll_addr, frame]);

                    // The frame now owns the GC args; drop their temporary roots.
                    if arg_roots > 0 {
                        self.emit_pop_roots_n(arg_roots);
                        self.gc_root_count -= arg_roots;
                    }

                    return frame;
                }
            }
        }

        self.builder.ins().iconst(types::I64, 0)
    }

    fn emit_ternary(&mut self, t: &TernaryExpr) -> cranelift_codegen::ir::Value {
        let result_ty = clif_type(&ast_type_of_ternary(t, &self.vars, self.func_return_types));
        let result_var = self.builder.declare_var(result_ty);

        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        let cond = self.emit_expr(&t.condition);
        self.builder
            .ins()
            .brif(cond, then_block, &[], else_block, &[]);

        // then branch — only this runs when condition is true (lazy)
        self.builder.switch_to_block(then_block);
        self.builder.seal_block(then_block);
        let then_val = self.emit_expr(&t.then_expr);
        self.builder.def_var(result_var, then_val);
        self.builder.ins().jump(merge_block, &[]);

        // else branch — only this runs when condition is false (lazy)
        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        let else_val = self.emit_expr(&t.else_expr);
        self.builder.def_var(result_var, else_val);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
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

    /// Convert error `e1_payload` (static class `e1_name`, implementing
    /// `Into<E2>`) to `E2` by calling `into`, dispatching VIRTUALLY on the
    /// payload's runtime type so a subclass override is honored (willow-bpk6).
    fn emit_into_conversion(
        &mut self,
        e1_payload: cranelift_codegen::ir::Value,
        e1_name: &str,
    ) -> cranelift_codegen::ir::Value {
        // Candidate runtime types: e1_name and its subclasses that resolve `into`.
        let mut dispatch: Vec<(i64, FuncId)> = self
            .class_type_ids
            .iter()
            .filter(|(cls, _)| self.class_is_a(cls, e1_name))
            .filter_map(|(cls, &id)| {
                self.resolve_method_func_id(cls, "into")
                    .map(|fid| (id, fid))
            })
            .collect();
        dispatch.sort_by_key(|(id, _)| *id);

        // Zero or one candidate: a plain direct call (no subclass override).
        if dispatch.len() <= 1 {
            let fid = dispatch
                .first()
                .map(|(_, f)| *f)
                .or_else(|| self.resolve_method_func_id(e1_name, "into"))
                .expect("Into impl must exist (verified by the type checker)");
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self.builder.ins().call(fref, &[e1_payload]);
            return self.builder.inst_results(call)[0];
        }

        // Multiple candidates: switch on the payload's runtime type_id (word 0).
        let type_id = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), e1_payload, 0i32);
        let result_var = self.builder.declare_var(types::I64);
        let zero = self.builder.ins().iconst(types::I64, 0);
        self.builder.def_var(result_var, zero);
        let merge = self.builder.create_block();
        let n = dispatch.len();
        for (i, (tid, fid)) in dispatch.into_iter().enumerate() {
            let tid_c = self.builder.ins().iconst(types::I64, tid);
            let is_match = self.builder.ins().icmp(IntCC::Equal, type_id, tid_c);
            let arm = self.builder.create_block();
            let next = self.builder.create_block();
            self.builder.ins().brif(is_match, arm, &[], next, &[]);
            self.builder.switch_to_block(arm);
            self.builder.seal_block(arm);
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self.builder.ins().call(fref, &[e1_payload]);
            let r = self.builder.inst_results(call)[0];
            self.builder.def_var(result_var, r);
            self.builder.ins().jump(merge, &[]);
            self.builder.switch_to_block(next);
            self.builder.seal_block(next);
            if i + 1 == n {
                self.builder.ins().jump(merge, &[]);
            }
        }
        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Lower `expr?` into control flow:
    /// - Result::Ok / Option::Some (tag == 0): extract and return the payload.
    /// - Result::Err / Option::None (tag == 1): early-return the enum pointer.
    fn emit_try_propagate(&mut self, inner: &Expr) -> cranelift_codegen::ir::Value {
        let operand_ty = self.ast_type_of(inner);
        let result_ptr = self.emit_expr(inner);
        let payload_ty = try_propagate_payload_type(&operand_ty);

        // Automatic error conversion (willow-1ow): if the operand's error type
        // `E1` differs from the enclosing function's error type `E2` (and neither
        // is `void`), the type checker has already verified `E1: Into<E2>`. On
        // the Err path we must convert `e1.into() -> e2` and re-wrap `Err(e2)`,
        // rather than returning the original `Result<_, E1>`.
        let convert: Option<(String, Type)> = match (
            result_err_type(&operand_ty),
            result_err_type(&self.return_type),
        ) {
            (Some(Type::Named(e1)), Some(e2))
                if Type::Named(e1.clone()) != e2 && e2 != Type::Void =>
            {
                Some((e1, e2))
            }
            _ => None,
        };

        // Load the enum tag from word 0.
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), result_ptr, 0i32);
        let ok_tag = self.builder.ins().iconst(types::I64, 0); // Ok = tag 0
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        // ── Propagate branch: early-return the Err ────────────────────────────
        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        // When the error types differ, convert `e1.into() -> e2` and re-wrap.
        let return_ptr = if let Some((e1_name, e2_ty)) = &convert {
            let e1_payload = self
                .builder
                .ins()
                .load(types::I64, MemFlags::new(), result_ptr, 8i32);
            let e1_is_gc = is_gc_managed(&Type::Named(e1_name.clone()), self.enum_infos);
            if e1_is_gc {
                self.emit_push_root(e1_payload);
            }
            // Dispatch `into` on the payload's runtime type so a subclassed
            // error that overrides `into` converts correctly (willow-bpk6).
            let e2_val = self.emit_into_conversion(e1_payload, e1_name);
            if e1_is_gc {
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
            }
            self.emit_alloc_enum_variant(1, e2_ty, e2_val)
        } else {
            result_ptr
        };
        if self.main_result_err_ty.is_some() {
            // In a `Result<void, E>` main, an Err is reported and exits non-zero
            // rather than being returned (willow_user_main is void). Roots are
            // popped inside emit_main_result_exit.
            self.emit_main_result_exit(return_ptr);
        } else {
            if self.gc_root_count > 0 {
                self.emit_pop_roots_n(self.gc_root_count);
            }
            // Return the (possibly converted) Result/Option pointer.
            self.builder.ins().return_(&[return_ptr]);
        }

        // ── Success branch: extract payload from word 1 ───────────────────────
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let payload = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), result_ptr, 8i32);
        self.coerce_i64_to(payload, &payload_ty)
    }

    /// Leave a `Result<void, E>` main by inspecting the `Result` value: `Err`
    /// reports the payload and exits non-zero; `Ok` returns void (exit 0). Pops
    /// the function's GC roots first (we leave the function on both paths).
    /// See willow-exg.
    fn emit_main_result_exit(&mut self, result_ptr: cranelift_codegen::ir::Value) {
        if self.gc_root_count > 0 {
            self.emit_pop_roots_n(self.gc_root_count);
        }
        let err_is_string = self.main_result_err_ty.as_ref() == Some(&Type::String);

        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), result_ptr, 0i32);
        let err_tag = self.builder.ins().iconst(types::I64, 1); // Err = tag 1
        let is_err = self.builder.ins().icmp(IntCC::Equal, tag, err_tag);
        let err_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_err, err_block, &[], ok_block, &[]);

        // Err: print the payload (a WillowString for E=String, else a generic
        // report via a null message) and exit non-zero.
        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        let msg = if err_is_string {
            self.builder
                .ins()
                .load(types::I64, MemFlags::new(), result_ptr, 8i32)
        } else {
            self.builder.ins().iconst(types::I64, 0)
        };
        let fail_id = self.func_ids["willow_main_fail"];
        let fail_ref = self.module.declare_func_in_func(fail_id, self.builder.func);
        self.builder.ins().call(fail_ref, &[msg]);
        // willow_main_fail is noreturn; trap to satisfy the verifier.
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        // Ok: success — return void (process exits 0).
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        self.builder.ins().return_(&[]);
    }

    fn emit_match(&mut self, m: &MatchExpr) -> cranelift_codegen::ir::Value {
        let scrutinee = self.emit_expr(&m.scrutinee);
        let scrutinee_ast_type = self.ast_type_of(&m.scrutinee);

        // Determine the result type from match arms, accounting for pattern bindings.
        let result_ast_type = {
            let mut scratch = self.vars.clone();
            let mut found = Type::I64;
            'outer: for arm in &m.arms {
                match &arm.pattern {
                    Pattern::Binding { name, .. } => {
                        let sty = scrutinee_ast_type.clone();
                        scratch.insert(
                            name.clone(),
                            VarStorage::Value {
                                var: self.builder.declare_var(clif_type(&sty)),
                                ty: sty,
                            },
                        );
                    }
                    Pattern::EnumVariantTuple {
                        enum_name,
                        variant,
                        bindings,
                        ..
                    } => {
                        // Resolve actual payload types — for generic types like Option/Result,
                        // use the type argument from the scrutinee rather than the placeholder.
                        let pts = self.resolve_variant_payload_types(
                            enum_name,
                            variant,
                            &scrutinee_ast_type,
                        );
                        for (name, ty) in bindings.iter().zip(pts.iter()) {
                            scratch.insert(
                                name.clone(),
                                VarStorage::Value {
                                    var: self.builder.declare_var(clif_type(ty)),
                                    ty: ty.clone(),
                                },
                            );
                        }
                    }
                    Pattern::ClassDowncast {
                        class_name,
                        binding,
                        ..
                    } if binding != "_" => {
                        let ty = Type::Named(class_name.clone());
                        scratch.insert(
                            binding.clone(),
                            VarStorage::Value {
                                var: self.builder.declare_var(clif_type(&ty)),
                                ty,
                            },
                        );
                    }
                    _ => {}
                }
                let ty = match &arm.body {
                    MatchBody::Expr(e) => ast_type_of_expr(e, &scratch, self.func_return_types),
                    MatchBody::Block(_) => Type::Void,
                };
                if ty != Type::Void && ty != Type::Never {
                    found = ty;
                    break 'outer;
                }
            }
            found
        };
        let result_clif_type = clif_type(&result_ast_type);
        let result_var = self.builder.declare_var(result_clif_type);
        let zero = if result_clif_type == types::F64 {
            let bits = self.builder.ins().iconst(types::I64, 0);
            self.builder
                .ins()
                .bitcast(types::F64, MemFlags::new(), bits)
        } else if result_clif_type == types::I8 {
            self.builder.ins().iconst(types::I8, 0)
        } else {
            self.builder.ins().iconst(types::I64, 0)
        };
        self.builder.def_var(result_var, zero);

        let merge_block = self.builder.create_block();

        let mut remaining = m.arms.as_slice();

        while !remaining.is_empty() {
            let arm = &remaining[0];
            remaining = &remaining[1..];

            let is_last = remaining.is_empty();

            let always_matches =
                matches!(arm.pattern, Pattern::Wildcard(_) | Pattern::Binding { .. });

            let arm_block = self.builder.create_block();
            let next_block = if always_matches || is_last {
                None
            } else {
                Some(self.builder.create_block())
            };

            if always_matches {
                self.builder.ins().jump(arm_block, &[]);
            } else {
                let cond = self.emit_pattern_check(scrutinee, &arm.pattern);
                let fallthrough = next_block.unwrap_or(merge_block);
                self.builder
                    .ins()
                    .brif(cond, arm_block, &[], fallthrough, &[]);
            }

            self.builder.switch_to_block(arm_block);
            self.builder.seal_block(arm_block);

            // For binding patterns, define the variable
            let saved_vars = match &arm.pattern {
                Pattern::Binding { name, .. } => {
                    let var = self.builder.declare_var(types::I64);
                    self.builder.def_var(var, scrutinee);
                    let saved = self.vars.clone();
                    self.vars.insert(
                        name.clone(),
                        VarStorage::Value {
                            var,
                            ty: result_ast_type.clone(),
                        },
                    );
                    Some(saved)
                }
                Pattern::EnumVariantTuple {
                    enum_name,
                    variant,
                    bindings,
                    ..
                } => {
                    let saved = self.vars.clone();
                    let payload_types =
                        self.resolve_variant_payload_types(enum_name, variant, &scrutinee_ast_type);
                    for (i, (binding, payload_ty)) in
                        bindings.iter().zip(payload_types.iter()).enumerate()
                    {
                        let offset = (1 + i) as i32 * 8;
                        let clif_ty = clif_type(payload_ty);
                        let raw =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), scrutinee, offset);
                        let val = if clif_ty == types::F64 {
                            self.builder.ins().bitcast(types::F64, MemFlags::new(), raw)
                        } else if clif_ty == types::I8 {
                            self.builder.ins().ireduce(types::I8, raw)
                        } else {
                            raw
                        };
                        let var = self.builder.declare_var(clif_ty);
                        self.builder.def_var(var, val);
                        self.vars.insert(
                            binding.clone(),
                            VarStorage::Value {
                                var,
                                ty: payload_ty.clone(),
                            },
                        );
                    }
                    Some(saved)
                }
                Pattern::ClassDowncast {
                    class_name,
                    binding,
                    ..
                } if binding != "_" => {
                    // Bind the downcast value: the box's object pointer (word 0),
                    // typed as the concrete class (willow-1js.4).
                    let saved = self.vars.clone();
                    let obj = self
                        .builder
                        .ins()
                        .load(types::I64, MemFlags::new(), scrutinee, 0i32);
                    let var = self.builder.declare_var(types::I64);
                    self.builder.def_var(var, obj);
                    self.vars.insert(
                        binding.clone(),
                        VarStorage::Value {
                            var,
                            ty: Type::Named(class_name.clone()),
                        },
                    );
                    Some(saved)
                }
                _ => None,
            };

            let outer_terminated = self.terminated;
            self.terminated = false;
            let arm_val = self.emit_match_body(&arm.body);

            if !self.terminated {
                self.builder.def_var(result_var, arm_val);
                self.builder.ins().jump(merge_block, &[]);
            }
            self.terminated = outer_terminated;

            if let Some(saved) = saved_vars {
                self.vars = saved;
            }

            if let Some(next) = next_block {
                self.builder.switch_to_block(next);
                self.builder.seal_block(next);
            }

            if always_matches {
                break;
            }
        }

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    fn emit_match_body(&mut self, body: &MatchBody) -> cranelift_codegen::ir::Value {
        match body {
            MatchBody::Expr(expr) => self.emit_expr(expr),
            MatchBody::Block(block) => {
                self.emit_block(block);
                self.builder.ins().iconst(types::I64, 0)
            }
        }
    }

    fn emit_pattern_check(
        &mut self,
        scrutinee: cranelift_codegen::ir::Value,
        pattern: &Pattern,
    ) -> cranelift_codegen::ir::Value {
        match pattern {
            Pattern::Wildcard(_) | Pattern::Binding { .. } => {
                self.builder.ins().iconst(types::I8, 1)
            }
            Pattern::LiteralBool(b, _) => {
                let expected = self.builder.ins().iconst(types::I8, if *b { 1 } else { 0 });
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            Pattern::LiteralInt(n, _) => {
                let expected = self.builder.ins().iconst(types::I64, *n);
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            Pattern::EnumVariant {
                enum_name, variant, ..
            } => {
                let tag = self.enum_variant_tag(enum_name, variant);
                let expected = self.builder.ins().iconst(types::I64, tag);
                if self.enum_is_gc_object_type(enum_name) {
                    let actual_tag = self.emit_load_enum_tag(scrutinee);
                    self.builder.ins().icmp(IntCC::Equal, actual_tag, expected)
                } else {
                    self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
                }
            }
            Pattern::EnumVariantTuple {
                enum_name, variant, ..
            } => {
                let tag = self.enum_variant_tag(enum_name, variant);
                let expected = self.builder.ins().iconst(types::I64, tag);
                let actual_tag = self.emit_load_enum_tag(scrutinee);
                self.builder.ins().icmp(IntCC::Equal, actual_tag, expected)
            }
            Pattern::ClassDowncast { class_name, .. } => {
                // The scrutinee is an interface box {object@0, vtable@8}. Match
                // when the boxed object's runtime type_id (object word 0) equals
                // the target class's type_id (willow-1js.4).
                let Some(&type_id) = self.class_type_ids.get(class_name) else {
                    return self.builder.ins().iconst(types::I8, 0); // unknown class: never matches
                };
                if self.build_mode == BuildMode::Debug {
                    self.emit_nil_check(scrutinee, pattern.span(), "interface downcast box");
                }
                let obj = self
                    .builder
                    .ins()
                    .load(types::I64, MemFlags::new(), scrutinee, 0i32);
                if self.build_mode == BuildMode::Debug {
                    self.emit_nil_check(obj, pattern.span(), "interface downcast object");
                }
                let actual = self
                    .builder
                    .ins()
                    .load(types::I64, MemFlags::new(), obj, 0i32);
                let expected = self.builder.ins().iconst(types::I64, type_id);
                self.builder.ins().icmp(IntCC::Equal, actual, expected)
            }
        }
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

    fn emit_load_enum_tag(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        self.builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32)
    }

    fn emit_enum_variant_alloc(
        &mut self,
        tag: i64,
        args: &[crate::parser::ast::CallArg],
    ) -> cranelift_codegen::ir::Value {
        let field_count = args.len();
        let total_words = 1 + field_count;
        let size = self
            .builder
            .ins()
            .iconst(types::I64, (total_words * 8) as i64);
        // Compute gc_ref_mask: layout is [tag_word, payload_0, payload_1, ...].
        // Word 0 = tag (never a GC ref).
        // Word i+1 = args[i]; set bit i+1 if that arg is GC-managed.
        let gc_mask: i64 = args.iter().enumerate().fold(0i64, |mask, (i, arg)| {
            if is_gc_managed(&self.ast_type_of(&arg.expr), self.enum_infos) {
                mask | (1i64 << (i + 1))
            } else {
                mask
            }
        });
        let mask = self.builder.ins().iconst(types::I64, gc_mask);
        let alloc_id = self.func_ids["willow_alloc_typed"];
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let ptr = self.builder.inst_results(call)[0];
        let tag_val = self.builder.ins().iconst(types::I64, tag);
        self.builder
            .ins()
            .store(MemFlags::new(), tag_val, ptr, 0i32);
        // Root the freshly allocated enum across argument evaluation: each
        // `emit_expr(&arg.expr)` below can allocate (e.g. a class payload), and
        // that allocation may trigger a collection.  Without this root the
        // half-built enum (tag stored, payload slots still zero) would be
        // reclaimed and we would store into freed memory.  The payload is
        // alloc_zeroed, so tracing the enum before all slots are stored is safe
        // (unstored ref slots read as null and are skipped).
        // Root whenever there is at least one argument to evaluate: even a
        // scalar-payload enum must survive an allocation inside an argument
        // expression (e.g. `Option::Some(f())` where `f` allocates internally).
        let needs_root = field_count > 0;
        if needs_root {
            self.emit_push_root(ptr);
        }
        for (i, arg) in args.iter().enumerate() {
            let offset = (1 + i) as i32 * 8;
            let val = self.emit_expr(&arg.expr);
            let val_i64 = if matches!(self.ast_type_of(&arg.expr), Type::F64) {
                self.builder.ins().bitcast(types::I64, MemFlags::new(), val)
            } else {
                val
            };
            self.builder
                .ins()
                .store(MemFlags::new(), val_i64, ptr, offset);
        }
        if needs_root {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        ptr
    }

    /// Load a `ClassName::property` static value from its global storage
    /// (willow-qsqf §13.4). The slot holds 8 bytes; the clif type comes from the
    /// property's declared type.
    fn emit_static_field_read(&mut self, class: &str, field: &str) -> cranelift_codegen::ir::Value {
        let class_name = self.static_call_class_name(class);
        if let Some(info) = self
            .static_storage
            .get(&(class_name.clone(), field.to_string()))
        {
            let ty = clif_type(&info.ty);
            let ptr_ty = self.module.target_config().pointer_type();
            let gv = self
                .module
                .declare_data_in_func(info.data_id, self.builder.func);
            let addr = self.builder.ins().global_value(ptr_ty, gv);
            return self.builder.ins().load(ty, MemFlags::new(), addr, 0);
        }
        // Should be unreachable after type checking; fall back to a zero value.
        self.builder.ins().iconst(types::I64, 0)
    }

    fn emit_static_call(&mut self, s: &StaticCallExpr) -> cranelift_codegen::ir::Value {
        let class_name = self.static_call_class_name(&s.class);

        // Built-in `Map::new()` constructor.
        if class_name == "Map" && s.method == "new" {
            let new_id = self.func_ids["willow_map_new"];
            let new_ref = self.module.declare_func_in_func(new_id, self.builder.func);
            let call = self.builder.ins().call(new_ref, &[]);
            return self.builder.inst_results(call)[0];
        }

        // Check if class is an enum — handle variant construction
        if let Some(enum_info) = self.enum_infos.get(&class_name).cloned() {
            if let Some(variant) = enum_info
                .variants
                .iter()
                .find(|v| v.name == s.method)
                .cloned()
            {
                if variant.payload_types.is_empty() && !self.enum_is_gc_object_type(&class_name) {
                    return self.builder.ins().iconst(types::I64, variant.tag);
                }
                if variant.payload_types.is_empty() {
                    return self.emit_enum_variant_alloc(variant.tag, &[]);
                }
                return self.emit_enum_variant_alloc(variant.tag, &s.args);
            }
        }

        if class_name == "Channel" && s.method == "new" {
            // Pass is_ref so the runtime can GC-trace the buffer for GC-element
            // channels (Channel<String>, Channel<class>, ...) (willow-dsw).
            let elem_ty = s.type_args.first().cloned().unwrap_or(Type::I64);
            let is_ref = is_gc_managed(&elem_ty, self.enum_infos);
            let fid = self.func_ids["willow_channel_new"];
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let flag = self.builder.ins().iconst(types::I64, is_ref as i64);
            let call = self.builder.ins().call(fref, &[flag]);
            return self.builder.inst_results(call)[0];
        }

        if class_name == "f64" && s.method == "to_string" {
            let fid = self.func_ids["willow_f64_to_string"];
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let (args, temp_roots) = self.emit_call_args_rooted(None, None, None, &s.args);
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            let result = results[0];
            if temp_roots > 0 {
                self.emit_pop_roots_n(temp_roots);
                self.gc_root_count -= temp_roots;
            }
            return result;
        }

        if class_name == "f64" && s.method == "parse" {
            let fid = self.func_ids["willow_f64_parse"];
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let (args, temp_roots) = self.emit_call_args_rooted(None, None, None, &s.args);
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            let result = results[0];
            if temp_roots > 0 {
                self.emit_pop_roots_n(temp_roots);
                self.gc_root_count -= temp_roots;
            }
            return result;
        }

        if class_name == "env" {
            let runtime_name = match s.method.as_str() {
                "args_len" => "willow_runtime_args_len",
                "arg" => "willow_runtime_arg",
                "program_name" => "willow_runtime_program_name",
                "args" => "willow_runtime_args_array",
                _ => "",
            };
            if !runtime_name.is_empty() {
                let fid = self.func_ids[runtime_name];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                let (args, temp_roots) = self.emit_call_args_rooted(None, None, None, &s.args);
                let call = self.builder.ins().call(fref, &args);
                let results = self.builder.inst_results(call);
                let result = if results.is_empty() {
                    self.builder.ins().iconst(types::I8, 0)
                } else {
                    results[0]
                };
                if temp_roots > 0 {
                    self.emit_pop_roots_n(temp_roots);
                    self.gc_root_count -= temp_roots;
                }
                return result;
            }
        }

        // Module call: `math::add(args)` → mangled name `math__add`
        if let Some(module_prefix) = self.known_modules.get(&class_name) {
            let mangled = format!("{}__{}", module_prefix, s.method);
            let fid = match self.func_ids.get(&mangled) {
                Some(&id) => id,
                None => panic!("undefined module function: {}", mangled),
            };
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let modes = self.func_param_modes.get(&mangled).cloned();
            let param_debug = self.func_param_debug.get(&mangled).cloned();
            let has_reference_args = has_reference_args(modes.as_deref(), &s.args);
            let user_callee = format!("{}::{}", class_name, s.method);
            let (args, temp_roots) = self.emit_call_args_rooted(
                Some(&user_callee),
                modes.as_deref(),
                param_debug.as_deref(),
                &s.args,
            );
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            let result = if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
            if has_reference_args {
                self.emit_debug_reference_call_clear();
            }
            self.emit_pop_roots_n(temp_roots);
            self.gc_root_count -= temp_roots;
            return result;
        }
        // Class static call: dispatch to the mangled class method function.
        // Class methods always have a hidden first `self` parameter (i64), so we
        // pass 0 (null) as the dummy self pointer for static (constructor-style) calls.
        let mangled = class_method_symbol_name(self.known_modules, &class_name, &s.method);
        if let Some(&fid) = self.func_ids.get(&mangled) {
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let dummy_self = self.builder.ins().iconst(types::I64, 0);
            let modes = self.func_param_modes.get(&mangled).cloned();
            let param_debug = self.func_param_debug.get(&mangled).cloned();
            let has_reference_args = has_reference_args(modes.as_deref(), &s.args);
            let user_callee = format!("{}::{}", class_name, s.method);
            let (arg_vals, temp_roots) = self.emit_call_args_rooted(
                Some(&user_callee),
                modes.as_deref(),
                param_debug.as_deref(),
                &s.args,
            );
            let mut args = vec![dummy_self];
            args.extend(arg_vals);
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            let result = if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
            if has_reference_args {
                self.emit_debug_reference_call_clear();
            }
            self.emit_pop_roots_n(temp_roots);
            self.gc_root_count -= temp_roots;
            return result;
        }
        self.builder.ins().iconst(types::I64, 0)
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

fn clif_type(ty: &Type) -> cranelift_codegen::ir::Type {
    match ty {
        Type::I64 => types::I64,
        Type::F64 => types::F64,
        Type::Bool => types::I8,
        Type::String => types::I64,
        Type::Nil => types::I64,
        Type::Never => types::I64, // bottom type — treated as I64 for codegen purposes
        Type::Array(_) => types::I64,
        // JoinHandle<T> is a pointer to a task data area — always I64.
        Type::Generic(name, _) if name == "JoinHandle" => types::I64,
        // Future<T> is an opaque runtime future pointer.
        Type::Generic(name, args) if name == "Future" && args.len() == 1 => types::I64,
        Type::Generic(_, _) => types::I64,
        Type::Nullable(_) => types::I64,
        Type::Fn(_, _) => types::I64, // function pointer (pointer-sized)
        Type::Named(_) => types::I64,
        Type::Void => types::I8,
    }
}

fn join_handle_result_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args) if name == "JoinHandle" && args.len() == 1 => {
            Some(args[0].clone())
        }
        _ => None,
    }
}

fn future_output_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args) if name == "Future" && args.len() == 1 => Some(args[0].clone()),
        _ => None,
    }
}

fn function_call_return_type(f: &FunctionDecl) -> Type {
    if f.is_async {
        Type::Generic("Future".to_string(), vec![f.return_type.clone()])
    } else {
        f.return_type.clone()
    }
}

fn method_call_return_type(m: &MethodDecl) -> Type {
    if m.is_async {
        Type::Generic("Future".to_string(), vec![m.return_type.clone()])
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

fn debug_type_name(ty: &Type) -> String {
    match ty {
        Type::I64 => "i64".to_string(),
        Type::F64 => "f64".to_string(),
        Type::Bool => "bool".to_string(),
        Type::String => "String".to_string(),
        Type::Void => "void".to_string(),
        Type::Nil => "nil".to_string(),
        Type::Never => "!".to_string(),
        Type::Named(name) => name.clone(),
        Type::Array(element) => format!("Array<{}>", debug_type_name(element)),
        Type::Generic(name, args) => {
            let args = args
                .iter()
                .map(debug_type_name)
                .collect::<Vec<_>>()
                .join(",");
            format!("{name}<{args}>")
        }
        Type::Nullable(inner) => format!("{}?", debug_type_name(inner)),
        Type::Fn(params, ret) => {
            let param_str = params
                .iter()
                .map(debug_type_name)
                .collect::<Vec<_>>()
                .join(",");
            format!("fn({}) -> {}", param_str, debug_type_name(ret))
        }
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

fn future_ready_runtime_name(ty: &Type) -> &'static str {
    match ty {
        Type::Void => "willow_future_ready_void",
        Type::I64 => "willow_future_ready_i64",
        Type::Bool => "willow_future_ready_bool",
        Type::F64 => "willow_future_ready_f64",
        _ => "willow_future_ready_ptr",
    }
}

fn future_await_runtime_name(ty: &Type) -> &'static str {
    match ty {
        Type::Void => "willow_future_await_void",
        Type::I64 => "willow_future_await_i64",
        Type::Bool => "willow_future_await_bool",
        Type::F64 => "willow_future_await_f64",
        _ => "willow_future_await_ptr",
    }
}

fn channel_element_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args) if name == "Channel" && args.len() == 1 => Some(args[0].clone()),
        _ => None,
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

/// Whether a Willow type is represented at runtime as a GC-managed heap pointer
/// (and therefore must be rooted when live across an allocation and traced when
/// stored inside another object).
///
/// `enum_infos` is required because a *fieldless* (C-like) enum — every variant
/// has no payload — is lowered to an immediate integer tag, NOT a heap pointer
/// (see `emit_static_call`).  Treating such a value as GC-managed would root or
/// trace a small integer as if it were an object pointer, and the collector
/// would dereference it as a header and crash.  An enum with at least one
/// payload-carrying variant is always heap-allocated and so is GC-managed.
fn is_gc_managed(ty: &Type, enum_infos: &HashMap<String, EnumInfo>) -> bool {
    match ty {
        Type::Named(name) => match enum_infos.get(name) {
            // Fieldless enum → immediate tag; with-payload enum → heap object.
            Some(info) => info.variants.iter().any(|v| !v.payload_types.is_empty()),
            // Classes and other named heap types.
            None => true,
        },
        // Array<T> is a GC-managed heap object (handle + buffer); locals,
        // parameters, and class fields of array type must be rooted/traced.
        Type::Array(_) => true,
        Type::Nullable(inner) => is_gc_managed(inner, enum_infos),
        // Channel/Future are opaque RUNTIME pointers (Box::into_raw / task-data
        // areas) with no willow_alloc_object GcHeader, so they must NOT be rooted
        // as heap objects — the collector would read a bogus header at
        // payload_to_header and crash (see willow-lpn.9). JoinHandle, however, is
        // currently represented by a GC async-frame pointer from `spawn`, so it
        // must be traced/rooted like any other frame.
        Type::Generic(name, _) if name == "Channel" || name == "Future" => false,
        Type::Generic(name, _) if name == "JoinHandle" => true,
        // Range<i64> is a real 2-word heap object (willow_alloc_typed, valid
        // GcHeader); its slots are plain i64 (mask 0) so nothing is traced, but
        // the value itself must be rooted like any heap object.
        Type::Generic(name, _) if name == "Range" => true,
        Type::Generic(_, _) => true,
        // String is now a GC-managed WillowString heap object (payload: len + bytes).
        // It is allocated via willow_alloc_typed and has a valid GcHeader.
        Type::String => true,
        _ => false,
    }
}

fn gc_ref_mask_for_layout(
    layout: &[(String, Type)],
    enum_infos: &HashMap<String, EnumInfo>,
) -> u64 {
    // Object layout: word 0 = type_id (not a GC ref), words 1..N = fields.
    // Bit i in the mask corresponds to word i; field[idx] lives at word (idx+1).
    // We only have 64 bits, so cap at 63 fields.
    layout
        .iter()
        .take(63)
        .enumerate()
        .fold(0u64, |mask, (idx, (_, ty))| {
            if is_gc_managed(ty, enum_infos) {
                mask | (1u64 << (idx + 1))
            } else {
                mask
            }
        })
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
        let gc_slot_mask = slots
            .iter()
            .take(64)
            .enumerate()
            .fold(0u64, |mask, (k, slot)| {
                if is_gc_managed(&slot.ty, enum_infos) {
                    mask | (1u64 << k)
                } else {
                    mask
                }
            });
        Self {
            slots,
            gc_slot_mask,
        }
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
                if let Some(ty) = &l.ty {
                    if seen.insert(l.span) {
                        out.push(AsyncFrameSlot {
                            key: l.span,
                            name: l.name.clone(),
                            ty: ty.clone(),
                        });
                    }
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

fn module_symbol_prefix(module_path: &str) -> String {
    module_path.split("::").collect::<Vec<_>>().join("__")
}

/// Make a `::`-qualified name safe for use inside a linker symbol.
fn backend_symbol_component(name: &str) -> String {
    name.replace("::", "__")
}

fn class_method_symbol_name(
    known_modules: &HashMap<String, String>,
    class_name: &str,
    method_name: &str,
) -> String {
    let module_match = known_modules
        .iter()
        .filter_map(|(access_name, symbol_prefix)| {
            class_name
                .strip_prefix(access_name)
                .and_then(|rest| rest.strip_prefix("::"))
                .map(|suffix| (access_name.len(), symbol_prefix, suffix))
        })
        .max_by_key(|(len, _, _)| *len);

    if let Some((_, symbol_prefix, class_suffix)) = module_match {
        let class_suffix = module_symbol_prefix(class_suffix);
        format!("{symbol_prefix}__{class_suffix}__{method_name}")
    } else {
        format!("{class_name}__{method_name}")
    }
}

fn qualify_module_class_decl(class: &ClassDecl, module_name: &str) -> ClassDecl {
    let mut qualified = class.clone();
    qualified.name = format!("{module_name}::{}", class.name);
    qualified.implements = class
        .implements
        .iter()
        .map(|iface| qualify_module_type(iface, module_name))
        .collect();
    qualified.fields = class
        .fields
        .iter()
        .map(|field| {
            let mut field = field.clone();
            field.ty = qualify_module_type(&field.ty, module_name);
            field
        })
        .collect();
    qualified.methods = class
        .methods
        .iter()
        .map(|method| {
            let mut method = method.clone();
            method.params = method
                .params
                .iter()
                .map(|param| {
                    let mut param = param.clone();
                    param.ty = qualify_module_type(&param.ty, module_name);
                    param
                })
                .collect();
            method.return_type = qualify_module_type(&method.return_type, module_name);
            method
        })
        .collect();
    qualified
}

fn qualify_module_type(ty: &Type, module_name: &str) -> Type {
    match ty {
        Type::Named(name) if !name.contains("::") => Type::Named(format!("{module_name}::{name}")),
        Type::Array(element) => Type::Array(Box::new(qualify_module_type(element, module_name))),
        Type::Generic(name, args) => Type::Generic(
            name.clone(),
            args.iter()
                .map(|arg| qualify_module_type(arg, module_name))
                .collect(),
        ),
        Type::Nullable(inner) => Type::Nullable(Box::new(qualify_module_type(inner, module_name))),
        Type::Fn(params, ret) => Type::Fn(
            params
                .iter()
                .map(|param| qualify_module_type(param, module_name))
                .collect(),
            Box::new(qualify_module_type(ret, module_name)),
        ),
        _ => ty.clone(),
    }
}

fn normalize_std_collection_program(program: &Program) -> Program {
    let imports = std_collection_imports(program);
    let mut program = program.clone();
    for item in &mut program.items {
        normalize_std_collection_item(item, &imports);
    }
    program
}

struct StdCollectionImports {
    modules: HashSet<String>,
    aliases: HashMap<String, String>,
}

fn std_collection_imports(program: &Program) -> StdCollectionImports {
    let mut modules = HashSet::new();
    let mut aliases = HashMap::new();
    for import in &program.imports {
        if !std_registry::is_std_path(&import.path) {
            continue;
        }
        match std_registry::resolve_std_import(&import.path, import.span) {
            Ok(std_registry::StdImport::Module { module })
                if module == "collections" && import.alias.is_none() =>
            {
                modules.insert("collections".to_string());
            }
            Ok(std_registry::StdImport::Item { module, item })
                if module == "collections" && matches!(item.as_str(), "Array" | "Map") =>
            {
                aliases.insert(import.alias.clone().unwrap_or_else(|| item.clone()), item);
            }
            _ => {}
        }
    }
    StdCollectionImports { modules, aliases }
}

fn normalize_std_collection_item(item: &mut Item, imports: &StdCollectionImports) {
    match item {
        Item::Function(function) => normalize_std_collection_function(function, imports),
        Item::Class(class) => {
            for field in &mut class.fields {
                normalize_std_collection_type(&mut field.ty, imports);
            }
            for method in &mut class.methods {
                normalize_std_collection_method(method, imports);
            }
        }
        Item::Enum(en) => {
            for variant in &mut en.variants {
                for ty in &mut variant.payload {
                    normalize_std_collection_type(ty, imports);
                }
            }
        }
        Item::Interface(interface) => {
            for method in &mut interface.methods {
                for param in &mut method.params {
                    normalize_std_collection_type(&mut param.ty, imports);
                }
                normalize_std_collection_type(&mut method.return_type, imports);
            }
        }
    }
}

fn normalize_std_collection_function(function: &mut FunctionDecl, imports: &StdCollectionImports) {
    for param in &mut function.params {
        normalize_std_collection_type(&mut param.ty, imports);
    }
    normalize_std_collection_type(&mut function.return_type, imports);
    normalize_std_collection_block(&mut function.body, imports);
}

fn normalize_std_collection_method(method: &mut MethodDecl, imports: &StdCollectionImports) {
    for param in &mut method.params {
        normalize_std_collection_type(&mut param.ty, imports);
    }
    normalize_std_collection_type(&mut method.return_type, imports);
    normalize_std_collection_block(&mut method.body, imports);
}

fn normalize_std_collection_block(block: &mut Block, imports: &StdCollectionImports) {
    for stmt in &mut block.stmts {
        normalize_std_collection_stmt(stmt, imports);
    }
}

fn normalize_std_collection_stmt(stmt: &mut Stmt, imports: &StdCollectionImports) {
    match stmt {
        Stmt::Let(s) => {
            if let Some(ty) = &mut s.ty {
                normalize_std_collection_type(ty, imports);
            }
            normalize_std_collection_expr(&mut s.init, imports);
        }
        Stmt::Assign(s) => normalize_std_collection_expr(&mut s.value, imports),
        Stmt::FieldAssign(s) => {
            normalize_std_collection_expr(&mut s.object, imports);
            normalize_std_collection_expr(&mut s.value, imports);
        }
        Stmt::IndexAssign(s) => {
            normalize_std_collection_expr(&mut s.array, imports);
            normalize_std_collection_expr(&mut s.index, imports);
            normalize_std_collection_expr(&mut s.value, imports);
        }
        Stmt::If(s) => {
            normalize_std_collection_expr(&mut s.cond, imports);
            normalize_std_collection_block(&mut s.then_block, imports);
            if let Some(block) = &mut s.else_block {
                normalize_std_collection_block(block, imports);
            }
        }
        Stmt::While(s) => {
            normalize_std_collection_expr(&mut s.cond, imports);
            normalize_std_collection_block(&mut s.body, imports);
        }
        Stmt::For(s) => {
            normalize_std_collection_expr(&mut s.iterable, imports);
            normalize_std_collection_block(&mut s.body, imports);
        }
        Stmt::Return(s) => {
            if let Some(value) = &mut s.value {
                normalize_std_collection_expr(value, imports);
            }
        }
        Stmt::Expr(s) => normalize_std_collection_expr(&mut s.expr, imports),
    }
}

fn normalize_std_collection_expr(expr: &mut Expr, imports: &StdCollectionImports) {
    match expr {
        Expr::StaticField(_) => {}
        Expr::Binary(binary) => {
            normalize_std_collection_expr(&mut binary.lhs, imports);
            normalize_std_collection_expr(&mut binary.rhs, imports);
        }
        Expr::Unary(unary) => normalize_std_collection_expr(&mut unary.expr, imports),
        Expr::Call(call) => {
            for arg in &mut call.args {
                normalize_std_collection_call_arg(arg, imports);
            }
        }
        Expr::FieldAccess(object, _, _) => {
            normalize_std_collection_expr(object, imports);
        }
        Expr::MethodCall(call) => {
            normalize_std_collection_expr(&mut call.object, imports);
            for arg in &mut call.args {
                normalize_std_collection_call_arg(arg, imports);
            }
        }
        Expr::StaticCall(call) => {
            if let Some(item) = std_collection_item_name(&call.class, imports) {
                call.class = item.to_string();
            }
            for ty in &mut call.type_args {
                normalize_std_collection_type(ty, imports);
            }
            for arg in &mut call.args {
                normalize_std_collection_call_arg(arg, imports);
            }
        }
        Expr::ObjectLiteral(object) => {
            for field in &mut object.fields {
                normalize_std_collection_expr(&mut field.value, imports);
            }
        }
        Expr::Spawn(spawn) => {
            for arg in &mut spawn.args {
                normalize_std_collection_call_arg(arg, imports);
            }
        }
        Expr::Await(await_expr) => {
            normalize_std_collection_expr(&mut await_expr.expr, imports);
        }
        Expr::Print(arg, _, _) => normalize_std_collection_expr(arg, imports),
        Expr::Ternary(ternary) => {
            normalize_std_collection_expr(&mut ternary.condition, imports);
            normalize_std_collection_expr(&mut ternary.then_expr, imports);
            normalize_std_collection_expr(&mut ternary.else_expr, imports);
        }
        Expr::Range(range) => {
            normalize_std_collection_expr(&mut range.start, imports);
            normalize_std_collection_expr(&mut range.end, imports);
        }
        Expr::Lambda(lambda) => {
            for param in &mut lambda.params {
                if let Some(ty) = &mut param.ty {
                    normalize_std_collection_type(ty, imports);
                }
            }
            if let Some(ty) = &mut lambda.return_type {
                normalize_std_collection_type(ty, imports);
            }
            match &mut lambda.body {
                LambdaBody::Expr(body) => normalize_std_collection_expr(body, imports),
                LambdaBody::Block(block) => normalize_std_collection_block(block, imports),
            }
        }
        Expr::Match(match_expr) => {
            normalize_std_collection_expr(&mut match_expr.scrutinee, imports);
            for arm in &mut match_expr.arms {
                match &mut arm.body {
                    MatchBody::Expr(body) => normalize_std_collection_expr(body, imports),
                    MatchBody::Block(block) => normalize_std_collection_block(block, imports),
                }
            }
        }
        Expr::TryPropagate(inner, _) => normalize_std_collection_expr(inner, imports),
        Expr::ArrayLiteral(elements, _) => {
            for element in elements {
                normalize_std_collection_expr(element, imports);
            }
        }
        Expr::Index(array, index, _) => {
            normalize_std_collection_expr(array, imports);
            normalize_std_collection_expr(index, imports);
        }
        Expr::Select(s) => {
            for case in &mut s.cases {
                match &mut case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        normalize_std_collection_expr(channel, imports)
                    }
                    SelectCaseKind::Send { channel, value } => {
                        normalize_std_collection_expr(channel, imports);
                        normalize_std_collection_expr(value, imports);
                    }
                    SelectCaseKind::Default => {}
                }
                normalize_std_collection_block(&mut case.body, imports);
            }
        }
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::String(_, _)
        | Expr::Var(_, _) => {}
    }
}

fn normalize_std_collection_call_arg(arg: &mut CallArg, imports: &StdCollectionImports) {
    normalize_std_collection_expr(&mut arg.expr, imports);
}

fn normalize_std_collection_type(ty: &mut Type, imports: &StdCollectionImports) {
    match ty {
        Type::Array(element) => normalize_std_collection_type(element, imports),
        Type::Generic(name, args) => {
            for arg in args.iter_mut() {
                normalize_std_collection_type(arg, imports);
            }
            match std_collection_item_name(name, imports) {
                Some("Array") if args.len() == 1 => {
                    let element = args.remove(0);
                    *ty = Type::Array(Box::new(element));
                }
                Some("Map") => {
                    *name = "Map".to_string();
                }
                Some("Option") => {
                    *name = "Option".to_string();
                }
                Some("Result") => {
                    *name = "Result".to_string();
                }
                _ => {}
            }
        }
        Type::Nullable(inner) => normalize_std_collection_type(inner, imports),
        Type::Fn(params, ret) => {
            for param in params {
                normalize_std_collection_type(param, imports);
            }
            normalize_std_collection_type(ret, imports);
        }
        Type::I64
        | Type::F64
        | Type::Bool
        | Type::String
        | Type::Void
        | Type::Nil
        | Type::Named(_)
        | Type::Never => {}
    }
}

fn std_collection_item_name<'a>(
    qualified_name: &'a str,
    imports: &'a StdCollectionImports,
) -> Option<&'a str> {
    if let Some(item) = imports.aliases.get(qualified_name) {
        return Some(item.as_str());
    }
    match qualified_name {
        "std::collections::Array" => return Some("Array"),
        "std::collections::Map" => return Some("Map"),
        "std::option::Option" => return Some("Option"),
        "std::result::Result" => return Some("Result"),
        _ => {}
    }
    let (module, item) = qualified_name.split_once("::")?;
    if imports.modules.contains(module) && matches!(item, "Array" | "Map") {
        Some(item)
    } else {
        None
    }
}

fn class_name_for_object_type(ty: &Type) -> Option<String> {
    match ty {
        Type::Named(name) => Some(name.clone()),
        Type::Nullable(inner) => class_name_for_object_type(inner),
        _ => None,
    }
}

fn user_function_symbol(name: &str) -> String {
    if name == "main" {
        USER_MAIN_SYMBOL.to_string()
    } else {
        name.to_string()
    }
}

/// Whether an expression is `Result::Ok()` with no arguments — the success
/// value for a `Result<void, E>` main, which carries no payload (willow-exg).
fn is_zero_arg_result_ok(expr: &Expr) -> bool {
    matches!(expr, Expr::StaticCall(s) if s.args.is_empty() && s.method == "Ok" && s.class == "Result")
}

/// If `expr` is `await sleep(<arg>)`, return the sleep argument (willow-lpn.5.3).
fn await_sleep_arg(expr: &Expr) -> Option<&Expr> {
    if let Expr::Await(a) = expr {
        if let Expr::Call(c) = &a.expr {
            if c.callee == "sleep" && c.args.len() == 1 {
                return Some(&c.args[0].expr);
            }
        }
    }
    None
}

/// If `expr` is `await <call>` where the callee is a cooperative leaf, return
/// the call (and the await's span) — the suspendable call-await form. Used by
/// the cooperative awaiter lowering (willow-lpn.5.3.1).
fn await_coop_call<'a>(
    expr: &'a Expr,
    cooperative_leaves: &std::collections::HashSet<String>,
) -> Option<(&'a CallExpr, crate::diagnostics::Span)> {
    if let Expr::Await(a) = expr {
        if let Expr::Call(c) = &a.expr {
            if cooperative_leaves.contains(&c.callee) {
                return Some((c, a.span));
            }
        }
    }
    None
}

/// If `expr` is a top-level channel `recv()` (`ch.recv()`), return the method
/// call. A cooperative `recv` is a suspend point: the task parks as a channel
/// waiter when empty and is woken by `send`/`close` (willow-dsw).
fn is_channel_recv(expr: &Expr) -> Option<&MethodCallExpr> {
    if let Expr::MethodCall(m) = expr {
        if m.method == "recv" && m.args.is_empty() {
            return Some(m);
        }
    }
    None
}

/// Conservatively true if `expr` contains an `await` anywhere. Unknown
/// expression shapes return `true` so the cooperative gate stays safe (falls
/// back to the eager path) (willow-lpn.5.3).
fn expr_contains_await(expr: &Expr) -> bool {
    match expr {
        Expr::Await(_) => true,
        Expr::Integer(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::String(..)
        | Expr::Var(..)
        | Expr::Nil(..) => false,
        Expr::Print(inner, _, _) => expr_contains_await(inner),
        Expr::Call(c) => c.args.iter().any(|a| expr_contains_await(&a.expr)),
        Expr::MethodCall(m) => {
            expr_contains_await(&m.object) || m.args.iter().any(|a| expr_contains_await(&a.expr))
        }
        Expr::StaticCall(s) => s.args.iter().any(|a| expr_contains_await(&a.expr)),
        Expr::Binary(b) => expr_contains_await(&b.lhs) || expr_contains_await(&b.rhs),
        Expr::Unary(u) => expr_contains_await(&u.expr),
        Expr::Ternary(t) => {
            expr_contains_await(&t.condition)
                || expr_contains_await(&t.then_expr)
                || expr_contains_await(&t.else_expr)
        }
        Expr::Range(r) => expr_contains_await(&r.start) || expr_contains_await(&r.end),
        Expr::FieldAccess(obj, _, _) => expr_contains_await(obj),
        Expr::ObjectLiteral(o) => o.fields.iter().any(|f| expr_contains_await(&f.value)),
        Expr::TryPropagate(inner, _) => expr_contains_await(inner),
        Expr::ArrayLiteral(elements, _) => elements.iter().any(expr_contains_await),
        Expr::Index(arr, index, _) => expr_contains_await(arr) || expr_contains_await(index),
        _ => true,
    }
}

/// Strict eligibility for the cooperative `async fn main` lowering (willow-lpn.5.3
/// Stage 2): an `async fn main()` with no params and `void` return whose body is
/// a flat sequence where every statement is a non-await expression statement,
/// `await sleep(n);`, or a `let x = <non-await expr>;` whose local is GC-managed
/// (so it is frame-backed and survives suspension), with at least one
/// at least one suspend point. Anything else (non-GC locals, control flow,
/// unsupported async-call awaits, spawn, channels) keeps the eager lowering.
fn cooperative_main_eligible(
    f: &FunctionDecl,
    async_local_types: &HashMap<crate::diagnostics::Span, Type>,
    enum_infos: &HashMap<String, EnumInfo>,
    cooperative_leaves: &std::collections::HashSet<String>,
) -> bool {
    let _ = enum_infos;
    if !f.is_async || f.name != "main" || !f.params.is_empty() || f.return_type != Type::Void {
        return false;
    }
    let mut has_sleep = false;
    let mut has_return = false;
    if !coop_stmts_eligible(
        &f.body.stmts,
        async_local_types,
        cooperative_leaves,
        false,
        &mut has_sleep,
        &mut has_return,
    ) {
        return false;
    }
    has_sleep
}

/// Eligibility for the cooperative LEAF lowering (willow-lpn.5.3 slices 4/4b/4c):
/// an `async fn` (not `main`) with by-value params and a non-void return whose
/// body is cooperatively lowerable (see [`coop_stmts_eligible`]) with at least
/// one suspend point and at least one value `return`. Such a fn compiles to a
/// constructor + suspending poll fn and is awaited via block-on.
fn cooperative_leaf_eligible(
    f: &FunctionDecl,
    async_local_types: &HashMap<crate::diagnostics::Span, Type>,
    cooperative_leaves: &std::collections::HashSet<String>,
) -> bool {
    if !f.is_async || f.name == "main" || f.return_type == Type::Void {
        return false;
    }
    // Only by-value params: each is stored into a frame slot by the constructor.
    if f.params.iter().any(|p| p.mode != ParamMode::Value) {
        return false;
    }
    let mut has_sleep = false;
    let mut has_return = false;
    if !coop_stmts_eligible(
        &f.body.stmts,
        async_local_types,
        cooperative_leaves,
        true,
        &mut has_sleep,
        &mut has_return,
    ) {
        return false;
    }
    has_sleep && has_return
}

/// Shared cooperative-lowering eligibility check over a statement sequence
/// (willow-lpn.5.3 slice 5): each statement must be a non-await expression
/// statement, `await sleep(n);`, a `let x = <non-await>;` with a resolvable
/// type, an await-free assignment, field/index assignments with await-free
/// targets and an optional call-await value, a `return` (bare always, value only
/// when `allow_value_return`), or an `if`/`while` whose condition is await-free
/// and whose bodies recursively satisfy the same rules. Sets
/// `has_sleep`/`has_return` if seen.
fn coop_stmts_eligible(
    stmts: &[Stmt],
    async_local_types: &HashMap<crate::diagnostics::Span, Type>,
    cooperative_leaves: &std::collections::HashSet<String>,
    allow_value_return: bool,
    has_sleep: &mut bool,
    has_return: &mut bool,
) -> bool {
    for stmt in stmts {
        match stmt {
            Stmt::Expr(es) => {
                if await_sleep_arg(&es.expr).is_some() {
                    *has_sleep = true;
                } else if await_coop_call(&es.expr, cooperative_leaves).is_some() {
                    // `await <coop-leaf-call>;` is a suspend point (willow-lpn.5.3.1).
                    *has_sleep = true;
                } else if is_channel_recv(&es.expr).is_some() {
                    // `ch.recv();` is a cooperative suspend point (willow-dsw).
                    *has_sleep = true;
                } else if expr_contains_await(&es.expr) {
                    return false;
                }
            }
            Stmt::Let(l) => {
                if l.name == "_" {
                    return false;
                }
                // `let x = await <coop-leaf-call>` / `let x = ch.recv()` are allowed
                // suspend points; any other await in the initializer is not lowerable.
                if await_coop_call(&l.init, cooperative_leaves).is_some()
                    || is_channel_recv(&l.init).is_some()
                {
                    *has_sleep = true;
                } else if expr_contains_await(&l.init) {
                    return false;
                }
                if l.ty.is_none() && !async_local_types.contains_key(&l.span) {
                    return false;
                }
            }
            Stmt::Assign(a) => {
                if await_coop_call(&a.value, cooperative_leaves).is_some()
                    || is_channel_recv(&a.value).is_some()
                {
                    *has_sleep = true;
                } else if expr_contains_await(&a.value) {
                    return false;
                }
            }
            Stmt::FieldAssign(s) => {
                if expr_contains_await(&s.object) {
                    return false;
                }
                if await_coop_call(&s.value, cooperative_leaves).is_some() {
                    *has_sleep = true;
                } else if expr_contains_await(&s.value) {
                    return false;
                }
            }
            Stmt::IndexAssign(s) => {
                if expr_contains_await(&s.array) || expr_contains_await(&s.index) {
                    return false;
                }
                if await_coop_call(&s.value, cooperative_leaves).is_some() {
                    *has_sleep = true;
                } else if expr_contains_await(&s.value) {
                    return false;
                }
            }
            Stmt::Return(r) => match &r.value {
                None => {}
                Some(v)
                    if allow_value_return && await_coop_call(v, cooperative_leaves).is_some() =>
                {
                    *has_sleep = true;
                    *has_return = true;
                }
                Some(v) if allow_value_return && !expr_contains_await(v) => *has_return = true,
                _ => return false,
            },
            Stmt::If(s) => {
                if expr_contains_await(&s.cond) {
                    return false;
                }
                if !coop_stmts_eligible(
                    &s.then_block.stmts,
                    async_local_types,
                    cooperative_leaves,
                    allow_value_return,
                    has_sleep,
                    has_return,
                ) {
                    return false;
                }
                if let Some(eb) = &s.else_block {
                    if !coop_stmts_eligible(
                        &eb.stmts,
                        async_local_types,
                        cooperative_leaves,
                        allow_value_return,
                        has_sleep,
                        has_return,
                    ) {
                        return false;
                    }
                }
            }
            Stmt::While(s) => {
                if expr_contains_await(&s.cond) {
                    return false;
                }
                if !coop_stmts_eligible(
                    &s.body.stmts,
                    async_local_types,
                    cooperative_leaves,
                    allow_value_return,
                    has_sleep,
                    has_return,
                ) {
                    return false;
                }
            }
            Stmt::For(s) => {
                if expr_contains_await(&s.iterable) {
                    return false;
                }
                if !async_local_types.contains_key(&s.iter_frame_key())
                    || !async_local_types.contains_key(&s.index_frame_key())
                    || (s.name != "_" && !async_local_types.contains_key(&s.name_span))
                {
                    return false;
                }
                if !coop_stmts_eligible(
                    &s.body.stmts,
                    async_local_types,
                    cooperative_leaves,
                    allow_value_return,
                    has_sleep,
                    has_return,
                ) {
                    return false;
                }
            }
        }
    }
    true
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

fn clif_type_of_expr(
    expr: &Expr,
    vars: &HashMap<String, VarStorage>,
    frt: &HashMap<String, Type>,
) -> cranelift_codegen::ir::Type {
    clif_type(&ast_type_of_expr(expr, vars, frt))
}

fn ast_type_of_expr(
    expr: &Expr,
    vars: &HashMap<String, VarStorage>,
    frt: &HashMap<String, Type>,
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
                ast_type_of_expr(&b.lhs, vars, frt)
            }
            _ => Type::Bool,
        },
        Expr::Unary(u) => match &u.op {
            UnaryOp::Neg => ast_type_of_expr(&u.expr, vars, frt),
            UnaryOp::Not => Type::Bool,
        },
        Expr::Call(c) => frt
            .get(&c.callee)
            .cloned()
            .or_else(|| builtin_call_return_type(&c.callee))
            .unwrap_or(Type::I64),
        Expr::Print(_, _, _) => Type::Void,
        Expr::Ternary(t) => ast_type_of_ternary(t, vars, frt),
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
            let obj_ty = ast_type_of_expr(&m.object, vars, frt);
            if m.method == "join" {
                if let Some(result_ty) = join_handle_result_type(&obj_ty) {
                    return result_ty;
                }
            }
            if m.method == "recv" {
                if let Some(element_ty) = channel_element_type(&obj_ty) {
                    return element_ty;
                }
            }
            if let Type::Array(elem) = &obj_ty {
                match m.method.as_str() {
                    "len" => return Type::I64,
                    "pop" => return (**elem).clone(),
                    "push" => return Type::Void,
                    _ => {}
                }
            }
            if let Type::Generic(name, margs) = &obj_ty {
                if name == "Map" && margs.len() == 2 {
                    match m.method.as_str() {
                        "get" => {
                            return Type::Generic("Option".to_string(), vec![margs[1].clone()]);
                        }
                        "len" => return Type::I64,
                        "contains" => return Type::Bool,
                        _ => return Type::Void,
                    }
                }
            }
            Type::Void
        }
        Expr::ObjectLiteral(o) => Type::Named(o.class.clone()),
        Expr::Spawn(s) => {
            let ret_ty = frt.get(&s.callee).cloned().unwrap_or_else(|| {
                vars.get(s.callee.as_str())
                    .and_then(|storage| match storage.ty() {
                        Type::Fn(_, ret) => Some((**ret).clone()),
                        _ => None,
                    })
                    .unwrap_or(Type::Void)
            });
            // Spawning + joining yields the awaited result, so the handle wraps
            // the UNWRAPPED return type: `Future<T>` (async target) -> `T`. Mirrors
            // the type checker's check_spawn.
            let ret_ty = future_output_type(&ret_ty).unwrap_or(ret_ty);
            Type::Generic("JoinHandle".to_string(), vec![ret_ty])
        }
        Expr::Await(a) => future_output_type(&ast_type_of_expr(&a.expr, vars, frt))
            .unwrap_or_else(|| ast_type_of_expr(&a.expr, vars, frt)),
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
            let scrutinee_ty = ast_type_of_expr(&m.scrutinee, vars, frt);
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
                    MatchBody::Expr(e) => ast_type_of_expr(e, &arm_vars, frt),
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
            let inner_ty = ast_type_of_expr(inner, vars, frt);
            if let Type::Generic(name, args) = &inner_ty {
                if (name == "Result" || name == "Option") && !args.is_empty() {
                    return args[0].clone();
                }
            }
            Type::I64
        }
        Expr::ArrayLiteral(elements, _) => {
            let elem = elements
                .first()
                .map(|e| ast_type_of_expr(e, vars, frt))
                .unwrap_or(Type::Void);
            Type::Array(Box::new(elem))
        }
        Expr::Index(arr, _, _) => match ast_type_of_expr(arr, vars, frt) {
            Type::Array(elem) => *elem,
            _ => Type::I64,
        },
    }
}

fn ast_type_of_ternary(
    t: &TernaryExpr,
    vars: &HashMap<String, VarStorage>,
    frt: &HashMap<String, Type>,
) -> Type {
    let then_ty = ast_type_of_expr(&t.then_expr, vars, frt);
    let else_ty = ast_type_of_expr(&t.else_expr, vars, frt);

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

fn builtin_static_return_type(class: &str, type_args: &[Type], method: &str) -> Option<Type> {
    match (class, method) {
        ("Channel", "new") => Some(Type::Generic(
            "Channel".to_string(),
            vec![type_args.first().cloned().unwrap_or(Type::Void)],
        )),
        ("env", "args_len") => Some(Type::I64),
        ("env", "arg") => Some(Type::String),
        ("env", "program_name") => Some(Type::String),
        ("env", "args") => Some(Type::Array(Box::new(Type::String))),
        ("f64", "to_string") => Some(Type::String),
        ("f64", "parse") => Some(Type::Generic(
            "Result".to_string(),
            vec![Type::F64, Type::Named("ParseFloatError".to_string())],
        )),
        _ => None,
    }
}

fn builtin_call_return_type(callee: &str) -> Option<Type> {
    if callee == "panic" {
        return Some(Type::Never);
    }
    match callee {
        "pow" | "powf" => Some(Type::F64),
        "format" => Some(Type::String),
        "gc_allocated_bytes" => Some(Type::I64),
        "gc_collect" => Some(Type::Void),
        "sleep" => Some(Type::Generic("Future".to_string(), vec![Type::Void])),
        _ => None,
    }
}

/// Infer the return type of a lambda body expression without needing the full
/// VarStorage context. Only handles simple cases; falls back to I64 for complex ones.
fn infer_lambda_body_type(
    expr: &Expr,
    param_types: &HashMap<String, Type>,
    frt: &HashMap<String, Type>,
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

fn builtin_call_runtime_name(callee: &str) -> Option<&'static str> {
    match callee {
        "pow" | "powf" => Some("willow_pow_f64"),
        "gc_collect" => Some("willow_gc_collect"),
        "gc_allocated_bytes" => Some("willow_gc_allocated_bytes"),
        "sleep" => Some("willow_runtime_sleep"),
        _ => None,
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
                    .map(|(i, (vn, pts))| EnumVariantInfo {
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
    //     traced from a frame slot; JoinHandle is a GC async frame and IS traced.
    #[test]
    fn async_frame_13_runtime_pointer_generics_and_joinhandle() {
        let future = Type::Generic("Future".to_string(), vec![Type::I64]);
        let channel = Type::Generic("Channel".to_string(), vec![Type::String]);
        let join = Type::Generic("JoinHandle".to_string(), vec![Type::Void]);
        assert_eq!(frame_layout(&[("f", future)]).gc_slot_mask, 0);
        assert_eq!(frame_layout(&[("c", channel)]).gc_slot_mask, 0);
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

    // 20. Slots beyond 64 are capped (mask is only 64 bits wide).
    #[test]
    fn async_frame_20_slots_capped_at_64() {
        let mut slots: Vec<(&str, Type)> = Vec::new();
        for _ in 0..70 {
            slots.push(("r", Type::String));
        }
        let layout = frame_layout(&slots);
        // All 64 representable bits set; slots 64..70 are dropped from the mask.
        assert_eq!(layout.gc_slot_mask, u64::MAX);
        assert!(!layout.slot_is_gc_ref(64));
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
    fn unit_async_codegen_08_async_function_call_returns_future_type() {
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
            Type::Generic("Future".to_string(), vec![Type::I64])
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
}

// ── Reference debug string collection helpers ────────────────────────────────

fn collect_reference_debug_strings_in_program(program: &Program) -> Vec<String> {
    let mut out = HashSet::new();
    for value in [
        "<unknown>",
        "&",
        "&mut",
        "value",
        "local",
        "field",
        "array_element",
        "expression",
    ] {
        out.insert(value.to_string());
    }

    for item in &program.items {
        match item {
            Item::Function(f) => {
                out.insert(f.name.clone());
                collect_reference_debug_param_strings(&f.params, &mut out);
                collect_reference_debug_strings_in_block(&f.body, &mut out);
            }
            Item::Class(c) => {
                for method in &c.methods {
                    out.insert(format!("{}::{}", c.name, method.name));
                    out.insert(method.name.clone());
                    collect_reference_debug_param_strings(&method.params, &mut out);
                    collect_reference_debug_strings_in_block(&method.body, &mut out);
                }
            }
            Item::Enum(_) => {}
            Item::Interface(_) => {} // no bodies
        }
    }

    out.into_iter().collect()
}

fn collect_reference_debug_param_strings(params: &[Param], out: &mut HashSet<String>) {
    for param in params {
        out.insert(param.name.clone());
        out.insert(debug_type_name(&param.ty));
        out.insert(reference_mode_name(&param.mode).to_string());
    }
}

fn collect_reference_debug_strings_in_block(block: &Block, out: &mut HashSet<String>) {
    for stmt in &block.stmts {
        collect_reference_debug_strings_in_stmt(stmt, out);
    }
}

fn collect_reference_debug_strings_in_stmt(stmt: &Stmt, out: &mut HashSet<String>) {
    match stmt {
        Stmt::Let(s) => collect_reference_debug_strings_in_expr(&s.init, out),
        Stmt::Assign(s) => collect_reference_debug_strings_in_expr(&s.value, out),
        Stmt::FieldAssign(s) => {
            collect_reference_debug_strings_in_expr(&s.object, out);
            collect_reference_debug_strings_in_expr(&s.value, out);
        }
        Stmt::IndexAssign(s) => {
            collect_reference_debug_strings_in_expr(&s.array, out);
            collect_reference_debug_strings_in_expr(&s.index, out);
            collect_reference_debug_strings_in_expr(&s.value, out);
        }
        Stmt::If(s) => {
            collect_reference_debug_strings_in_expr(&s.cond, out);
            collect_reference_debug_strings_in_block(&s.then_block, out);
            if let Some(else_block) = &s.else_block {
                collect_reference_debug_strings_in_block(else_block, out);
            }
        }
        Stmt::While(s) => {
            collect_reference_debug_strings_in_expr(&s.cond, out);
            collect_reference_debug_strings_in_block(&s.body, out);
        }
        Stmt::For(s) => {
            collect_reference_debug_strings_in_expr(&s.iterable, out);
            collect_reference_debug_strings_in_block(&s.body, out);
        }
        Stmt::Return(s) => {
            if let Some(value) = &s.value {
                collect_reference_debug_strings_in_expr(value, out);
            }
        }
        Stmt::Expr(s) => collect_reference_debug_strings_in_expr(&s.expr, out),
    }
}

fn collect_reference_debug_strings_in_expr(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::StaticField(_) => {}
        Expr::Call(c) => {
            collect_reference_debug_call_arg_strings(&c.callee, &c.args, out);
            for arg in &c.args {
                collect_reference_debug_strings_in_expr(&arg.expr, out);
            }
        }
        Expr::MethodCall(m) => {
            collect_reference_debug_strings_in_expr(&m.object, out);
            collect_reference_debug_call_arg_strings(&m.method, &m.args, out);
            for arg in &m.args {
                collect_reference_debug_strings_in_expr(&arg.expr, out);
            }
        }
        Expr::StaticCall(s) => {
            let callee = format!("{}::{}", s.class, s.method);
            collect_reference_debug_call_arg_strings(&callee, &s.args, out);
            for arg in &s.args {
                collect_reference_debug_strings_in_expr(&arg.expr, out);
            }
        }
        Expr::Binary(b) => {
            collect_reference_debug_strings_in_expr(&b.lhs, out);
            collect_reference_debug_strings_in_expr(&b.rhs, out);
        }
        Expr::Unary(u) => collect_reference_debug_strings_in_expr(&u.expr, out),
        Expr::FieldAccess(obj, _, _) => collect_reference_debug_strings_in_expr(obj, out),
        Expr::ObjectLiteral(o) => {
            for field in &o.fields {
                collect_reference_debug_strings_in_expr(&field.value, out);
            }
        }
        Expr::Spawn(s) => {
            for arg in &s.args {
                collect_reference_debug_strings_in_expr(&arg.expr, out);
            }
        }
        Expr::Await(a) => collect_reference_debug_strings_in_expr(&a.expr, out),
        Expr::Print(arg, _, _) => collect_reference_debug_strings_in_expr(arg, out),
        Expr::Ternary(t) => {
            collect_reference_debug_strings_in_expr(&t.condition, out);
            collect_reference_debug_strings_in_expr(&t.then_expr, out);
            collect_reference_debug_strings_in_expr(&t.else_expr, out);
        }
        Expr::Range(r) => {
            collect_reference_debug_strings_in_expr(&r.start, out);
            collect_reference_debug_strings_in_expr(&r.end, out);
        }
        Expr::Lambda(l) => match &l.body {
            LambdaBody::Expr(e) => collect_reference_debug_strings_in_expr(e, out),
            LambdaBody::Block(b) => collect_reference_debug_strings_in_block(b, out),
        },
        Expr::Match(m) => {
            collect_reference_debug_strings_in_expr(&m.scrutinee, out);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_reference_debug_strings_in_expr(e, out),
                    MatchBody::Block(b) => collect_reference_debug_strings_in_block(b, out),
                }
            }
        }
        Expr::TryPropagate(inner, _) => collect_reference_debug_strings_in_expr(inner, out),
        Expr::ArrayLiteral(elements, _) => {
            for el in elements {
                collect_reference_debug_strings_in_expr(el, out);
            }
        }
        Expr::Index(arr, index, _) => {
            collect_reference_debug_strings_in_expr(arr, out);
            collect_reference_debug_strings_in_expr(index, out);
        }
        Expr::Select(s) => {
            for case in &s.cases {
                match &case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        collect_reference_debug_strings_in_expr(channel, out)
                    }
                    SelectCaseKind::Send { channel, value } => {
                        collect_reference_debug_strings_in_expr(channel, out);
                        collect_reference_debug_strings_in_expr(value, out);
                    }
                    SelectCaseKind::Default => {}
                }
                collect_reference_debug_strings_in_block(&case.body, out);
            }
        }
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::String(_, _)
        | Expr::Var(_, _) => {}
    }
}

fn collect_reference_debug_call_arg_strings(
    callee: &str,
    args: &[CallArg],
    out: &mut HashSet<String>,
) {
    for arg in args {
        if matches!(&arg.mode, CallArgMode::Reference { .. }) {
            out.insert(callee.to_string());
            out.insert(reference_place_kind(&arg.expr).to_string());
            out.insert(reference_place_name(&arg.expr));
        }
    }
}

// ── String literal collection helpers ─────────────────────────────────────────

fn collect_string_literals_in_program(program: &Program) -> Vec<String> {
    let mut out = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) => collect_string_literals_in_block(&f.body, &mut out),
            Item::Class(c) => {
                for method in &c.methods {
                    collect_string_literals_in_block(&method.body, &mut out);
                }
                // Static-property initializers are emitted in __willow_static_init
                // (willow-qsqf), so their string literals must be declared too.
                for field in &c.fields {
                    if let Some(init) = &field.initializer {
                        collect_string_literals_in_expr(init, &mut out);
                    }
                }
            }
            Item::Enum(_) => {}
            Item::Interface(_) => {} // no bodies
        }
    }
    out
}

fn collect_string_literals_in_block(block: &Block, out: &mut Vec<String>) {
    for stmt in &block.stmts {
        collect_string_literals_in_stmt(stmt, out);
    }
}

fn collect_string_literals_in_stmt(stmt: &Stmt, out: &mut Vec<String>) {
    match stmt {
        Stmt::Let(s) => collect_string_literals_in_expr(&s.init, out),
        Stmt::Assign(s) => collect_string_literals_in_expr(&s.value, out),
        Stmt::FieldAssign(s) => {
            collect_string_literals_in_expr(&s.object, out);
            collect_string_literals_in_expr(&s.value, out);
        }
        Stmt::IndexAssign(s) => {
            collect_string_literals_in_expr(&s.array, out);
            collect_string_literals_in_expr(&s.index, out);
            collect_string_literals_in_expr(&s.value, out);
        }
        Stmt::If(s) => {
            collect_string_literals_in_expr(&s.cond, out);
            collect_string_literals_in_block(&s.then_block, out);
            if let Some(else_block) = &s.else_block {
                collect_string_literals_in_block(else_block, out);
            }
        }
        Stmt::While(s) => {
            collect_string_literals_in_expr(&s.cond, out);
            collect_string_literals_in_block(&s.body, out);
        }
        Stmt::For(s) => {
            collect_string_literals_in_expr(&s.iterable, out);
            collect_string_literals_in_block(&s.body, out);
        }
        Stmt::Return(s) => {
            if let Some(value) = &s.value {
                collect_string_literals_in_expr(value, out);
            }
        }
        Stmt::Expr(s) => collect_string_literals_in_expr(&s.expr, out),
    }
}

fn collect_string_literals_in_expr(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::StaticField(_) => {}
        Expr::String(value, _) => out.push(value.clone()),
        Expr::Binary(b) => {
            collect_string_literals_in_expr(&b.lhs, out);
            collect_string_literals_in_expr(&b.rhs, out);
        }
        Expr::Unary(u) => collect_string_literals_in_expr(&u.expr, out),
        Expr::Call(c) => {
            for arg in &c.args {
                collect_string_literals_in_expr(&arg.expr, out);
            }
        }
        Expr::FieldAccess(obj, _, _) => collect_string_literals_in_expr(obj, out),
        Expr::MethodCall(m) => {
            collect_string_literals_in_expr(&m.object, out);
            for arg in &m.args {
                collect_string_literals_in_expr(&arg.expr, out);
            }
        }
        Expr::StaticCall(s) => {
            for arg in &s.args {
                collect_string_literals_in_expr(&arg.expr, out);
            }
        }
        Expr::ObjectLiteral(o) => {
            for field in &o.fields {
                collect_string_literals_in_expr(&field.value, out);
            }
        }
        Expr::Spawn(s) => {
            for arg in &s.args {
                collect_string_literals_in_expr(&arg.expr, out);
            }
        }
        Expr::Await(a) => collect_string_literals_in_expr(&a.expr, out),
        Expr::Select(s) => {
            for case in &s.cases {
                match &case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        collect_string_literals_in_expr(channel, out)
                    }
                    SelectCaseKind::Send { channel, value } => {
                        collect_string_literals_in_expr(channel, out);
                        collect_string_literals_in_expr(value, out);
                    }
                    SelectCaseKind::Default => {}
                }
                collect_string_literals_in_block(&case.body, out);
            }
        }
        Expr::Print(arg, _, _) => collect_string_literals_in_expr(arg, out),
        Expr::Ternary(t) => {
            collect_string_literals_in_expr(&t.condition, out);
            collect_string_literals_in_expr(&t.then_expr, out);
            collect_string_literals_in_expr(&t.else_expr, out);
        }
        Expr::Range(r) => {
            collect_string_literals_in_expr(&r.start, out);
            collect_string_literals_in_expr(&r.end, out);
        }
        Expr::Lambda(l) => match &l.body {
            LambdaBody::Expr(e) => collect_string_literals_in_expr(e, out),
            LambdaBody::Block(b) => collect_string_literals_in_block(b, out),
        },
        Expr::Match(m) => {
            collect_string_literals_in_expr(&m.scrutinee, out);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_string_literals_in_expr(e, out),
                    MatchBody::Block(b) => collect_string_literals_in_block(b, out),
                }
            }
        }
        Expr::TryPropagate(inner, _) => collect_string_literals_in_expr(inner, out),
        Expr::ArrayLiteral(elements, _) => {
            for el in elements {
                collect_string_literals_in_expr(el, out);
            }
        }
        Expr::Index(arr, index, _) => {
            collect_string_literals_in_expr(arr, out);
            collect_string_literals_in_expr(index, out);
        }
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::Var(_, _) => {}
    }
}

// ── Lambda collection helpers ─────────────────────────────────────────────────

fn collect_lambdas_in_program(program: &Program) -> Vec<(String, LambdaExpr)> {
    let mut out = Vec::new();
    let mut counter = 0usize;
    for item in &program.items {
        match item {
            Item::Function(f) => collect_lambdas_in_block(&f.body, &mut counter, &mut out),
            Item::Class(c) => {
                for m in &c.methods {
                    collect_lambdas_in_block(&m.body, &mut counter, &mut out);
                }
            }
            Item::Enum(_) => {}
            Item::Interface(_) => {} // no bodies
        }
    }
    out
}

fn collect_lambdas_in_block(
    block: &Block,
    counter: &mut usize,
    out: &mut Vec<(String, LambdaExpr)>,
) {
    for stmt in &block.stmts {
        collect_lambdas_in_stmt(stmt, counter, out);
    }
}

fn collect_lambdas_in_stmt(stmt: &Stmt, counter: &mut usize, out: &mut Vec<(String, LambdaExpr)>) {
    match stmt {
        Stmt::Let(s) => collect_lambdas_in_expr(&s.init, counter, out),
        Stmt::Assign(s) => collect_lambdas_in_expr(&s.value, counter, out),
        Stmt::FieldAssign(s) => {
            collect_lambdas_in_expr(&s.object, counter, out);
            collect_lambdas_in_expr(&s.value, counter, out);
        }
        Stmt::IndexAssign(s) => {
            collect_lambdas_in_expr(&s.array, counter, out);
            collect_lambdas_in_expr(&s.index, counter, out);
            collect_lambdas_in_expr(&s.value, counter, out);
        }
        Stmt::If(s) => {
            collect_lambdas_in_expr(&s.cond, counter, out);
            collect_lambdas_in_block(&s.then_block, counter, out);
            if let Some(eb) = &s.else_block {
                collect_lambdas_in_block(eb, counter, out);
            }
        }
        Stmt::While(s) => {
            collect_lambdas_in_expr(&s.cond, counter, out);
            collect_lambdas_in_block(&s.body, counter, out);
        }
        Stmt::For(s) => {
            collect_lambdas_in_expr(&s.iterable, counter, out);
            collect_lambdas_in_block(&s.body, counter, out);
        }
        Stmt::Return(s) => {
            if let Some(v) = &s.value {
                collect_lambdas_in_expr(v, counter, out);
            }
        }
        Stmt::Expr(s) => collect_lambdas_in_expr(&s.expr, counter, out),
    }
}

fn collect_lambdas_in_expr(expr: &Expr, counter: &mut usize, out: &mut Vec<(String, LambdaExpr)>) {
    match expr {
        Expr::Lambda(l) => {
            // Recurse into the lambda body first so nested lambdas get lower IDs.
            match &l.body {
                LambdaBody::Block(b) => collect_lambdas_in_block(b, counter, out),
                LambdaBody::Expr(e) => collect_lambdas_in_expr(e, counter, out),
            }
            let name = format!("__lambda_{}", *counter);
            *counter += 1;
            out.push((name, *l.clone()));
        }
        Expr::Call(c) => {
            for arg in &c.args {
                collect_lambdas_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::Binary(b) => {
            collect_lambdas_in_expr(&b.lhs, counter, out);
            collect_lambdas_in_expr(&b.rhs, counter, out);
        }
        Expr::Unary(u) => collect_lambdas_in_expr(&u.expr, counter, out),
        Expr::Ternary(t) => {
            collect_lambdas_in_expr(&t.condition, counter, out);
            collect_lambdas_in_expr(&t.then_expr, counter, out);
            collect_lambdas_in_expr(&t.else_expr, counter, out);
        }
        Expr::Range(r) => {
            collect_lambdas_in_expr(&r.start, counter, out);
            collect_lambdas_in_expr(&r.end, counter, out);
        }
        Expr::Print(e, _, _) => collect_lambdas_in_expr(e, counter, out),
        Expr::StaticCall(s) => {
            for arg in &s.args {
                collect_lambdas_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::ObjectLiteral(o) => {
            for field in &o.fields {
                collect_lambdas_in_expr(&field.value, counter, out);
            }
        }
        Expr::Spawn(s) => {
            for arg in &s.args {
                collect_lambdas_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::Await(a) => collect_lambdas_in_expr(&a.expr, counter, out),
        Expr::Select(s) => {
            for case in &s.cases {
                match &case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        collect_lambdas_in_expr(channel, counter, out)
                    }
                    SelectCaseKind::Send { channel, value } => {
                        collect_lambdas_in_expr(channel, counter, out);
                        collect_lambdas_in_expr(value, counter, out);
                    }
                    SelectCaseKind::Default => {}
                }
                collect_lambdas_in_block(&case.body, counter, out);
            }
        }
        Expr::MethodCall(m) => {
            collect_lambdas_in_expr(&m.object, counter, out);
            for arg in &m.args {
                collect_lambdas_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::FieldAccess(e, _, _) => collect_lambdas_in_expr(e, counter, out),
        Expr::Match(m) => {
            collect_lambdas_in_expr(&m.scrutinee, counter, out);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_lambdas_in_expr(e, counter, out),
                    MatchBody::Block(b) => collect_lambdas_in_block(b, counter, out),
                }
            }
        }
        Expr::ArrayLiteral(elements, _) => {
            for el in elements {
                collect_lambdas_in_expr(el, counter, out);
            }
        }
        Expr::Index(arr, index, _) => {
            collect_lambdas_in_expr(arr, counter, out);
            collect_lambdas_in_expr(index, counter, out);
        }
        _ => {}
    }
}

// ── Spawn-site collection helpers ────────────────────────────────────────────
// Returns (span, tramp_name, callee_name) for every Expr::Spawn in the program.

fn collect_spawns_in_program(program: &Program) -> Vec<(crate::diagnostics::Span, String, String)> {
    let mut out = Vec::new();
    let mut counter = 0usize;
    for item in &program.items {
        match item {
            Item::Function(f) => collect_spawns_in_block(&f.body, &mut counter, &mut out),
            Item::Class(c) => {
                for m in &c.methods {
                    collect_spawns_in_block(&m.body, &mut counter, &mut out);
                }
            }
            Item::Enum(_) => {}
            Item::Interface(_) => {} // no bodies
        }
    }
    out
}

fn collect_spawns_in_block(
    block: &Block,
    counter: &mut usize,
    out: &mut Vec<(crate::diagnostics::Span, String, String)>,
) {
    for stmt in &block.stmts {
        collect_spawns_in_stmt(stmt, counter, out);
    }
}

fn collect_spawns_in_stmt(
    stmt: &Stmt,
    counter: &mut usize,
    out: &mut Vec<(crate::diagnostics::Span, String, String)>,
) {
    match stmt {
        Stmt::Let(s) => collect_spawns_in_expr(&s.init, counter, out),
        Stmt::Assign(s) => collect_spawns_in_expr(&s.value, counter, out),
        Stmt::FieldAssign(s) => {
            collect_spawns_in_expr(&s.object, counter, out);
            collect_spawns_in_expr(&s.value, counter, out);
        }
        Stmt::IndexAssign(s) => {
            collect_spawns_in_expr(&s.array, counter, out);
            collect_spawns_in_expr(&s.index, counter, out);
            collect_spawns_in_expr(&s.value, counter, out);
        }
        Stmt::If(s) => {
            collect_spawns_in_expr(&s.cond, counter, out);
            collect_spawns_in_block(&s.then_block, counter, out);
            if let Some(eb) = &s.else_block {
                collect_spawns_in_block(eb, counter, out);
            }
        }
        Stmt::While(s) => {
            collect_spawns_in_expr(&s.cond, counter, out);
            collect_spawns_in_block(&s.body, counter, out);
        }
        Stmt::For(s) => {
            collect_spawns_in_expr(&s.iterable, counter, out);
            collect_spawns_in_block(&s.body, counter, out);
        }
        Stmt::Return(s) => {
            if let Some(v) = &s.value {
                collect_spawns_in_expr(v, counter, out);
            }
        }
        Stmt::Expr(s) => collect_spawns_in_expr(&s.expr, counter, out),
    }
}

fn collect_spawns_in_expr(
    expr: &Expr,
    counter: &mut usize,
    out: &mut Vec<(crate::diagnostics::Span, String, String)>,
) {
    match expr {
        Expr::StaticField(_) => {}
        Expr::Spawn(s) => {
            let tramp_name = format!("__willow_spawn_tramp_{}", *counter);
            *counter += 1;
            out.push((s.span, tramp_name, s.callee.clone()));
            for arg in &s.args {
                collect_spawns_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::Call(c) => {
            for arg in &c.args {
                collect_spawns_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::Binary(b) => {
            collect_spawns_in_expr(&b.lhs, counter, out);
            collect_spawns_in_expr(&b.rhs, counter, out);
        }
        Expr::Unary(u) => collect_spawns_in_expr(&u.expr, counter, out),
        Expr::Ternary(t) => {
            collect_spawns_in_expr(&t.condition, counter, out);
            collect_spawns_in_expr(&t.then_expr, counter, out);
            collect_spawns_in_expr(&t.else_expr, counter, out);
        }
        Expr::Range(r) => {
            collect_spawns_in_expr(&r.start, counter, out);
            collect_spawns_in_expr(&r.end, counter, out);
        }
        Expr::Print(e, _, _) => collect_spawns_in_expr(e, counter, out),
        Expr::Await(a) => collect_spawns_in_expr(&a.expr, counter, out),
        Expr::MethodCall(m) => {
            collect_spawns_in_expr(&m.object, counter, out);
            for arg in &m.args {
                collect_spawns_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::FieldAccess(e, _, _) => collect_spawns_in_expr(e, counter, out),
        Expr::StaticCall(s) => {
            for arg in &s.args {
                collect_spawns_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::ObjectLiteral(o) => {
            for field in &o.fields {
                collect_spawns_in_expr(&field.value, counter, out);
            }
        }
        Expr::Lambda(l) => match &l.body {
            LambdaBody::Block(b) => collect_spawns_in_block(b, counter, out),
            LambdaBody::Expr(e) => collect_spawns_in_expr(e, counter, out),
        },
        Expr::Match(m) => {
            collect_spawns_in_expr(&m.scrutinee, counter, out);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_spawns_in_expr(e, counter, out),
                    MatchBody::Block(b) => collect_spawns_in_block(b, counter, out),
                }
            }
        }
        Expr::TryPropagate(inner, _) => collect_spawns_in_expr(inner, counter, out),
        Expr::ArrayLiteral(elements, _) => {
            for el in elements {
                collect_spawns_in_expr(el, counter, out);
            }
        }
        Expr::Index(arr, index, _) => {
            collect_spawns_in_expr(arr, counter, out);
            collect_spawns_in_expr(index, counter, out);
        }
        Expr::Select(s) => {
            for case in &s.cases {
                match &case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        collect_spawns_in_expr(channel, counter, out)
                    }
                    SelectCaseKind::Send { channel, value } => {
                        collect_spawns_in_expr(channel, counter, out);
                        collect_spawns_in_expr(value, counter, out);
                    }
                    SelectCaseKind::Default => {}
                }
                collect_spawns_in_block(&case.body, counter, out);
            }
        }
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::String(_, _)
        | Expr::Var(_, _) => {}
    }
}

// ── Nil-check string pre-scan ─────────────────────────────────────────────────
// Collect all field names and method names referenced in the program so their
// string literals can be pre-declared before any function is compiled.

fn collect_nil_check_names(program: &Program) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for item in &program.items {
        match item {
            Item::Function(f) => collect_nil_check_names_in_block(&f.body, &mut out),
            Item::Class(c) => {
                for m in &c.methods {
                    collect_nil_check_names_in_block(&m.body, &mut out);
                }
            }
            Item::Enum(_) => {}
            Item::Interface(_) => {} // no bodies
        }
    }
    out
}

fn collect_nil_check_names_in_block(block: &Block, out: &mut std::collections::HashSet<String>) {
    for stmt in &block.stmts {
        collect_nil_check_names_in_stmt(stmt, out);
    }
}

fn collect_nil_check_names_in_stmt(stmt: &Stmt, out: &mut std::collections::HashSet<String>) {
    match stmt {
        Stmt::Let(s) => collect_nil_check_names_in_expr(&s.init, out),
        Stmt::Assign(s) => collect_nil_check_names_in_expr(&s.value, out),
        Stmt::FieldAssign(s) => {
            collect_nil_check_names_in_expr(&s.object, out);
            collect_nil_check_names_in_expr(&s.value, out);
        }
        Stmt::IndexAssign(s) => {
            collect_nil_check_names_in_expr(&s.array, out);
            collect_nil_check_names_in_expr(&s.index, out);
            collect_nil_check_names_in_expr(&s.value, out);
        }
        Stmt::If(s) => {
            collect_nil_check_names_in_expr(&s.cond, out);
            collect_nil_check_names_in_block(&s.then_block, out);
            if let Some(eb) = &s.else_block {
                collect_nil_check_names_in_block(eb, out);
            }
        }
        Stmt::While(s) => {
            collect_nil_check_names_in_expr(&s.cond, out);
            collect_nil_check_names_in_block(&s.body, out);
        }
        Stmt::For(s) => {
            collect_nil_check_names_in_expr(&s.iterable, out);
            collect_nil_check_names_in_block(&s.body, out);
        }
        Stmt::Return(s) => {
            if let Some(v) = &s.value {
                collect_nil_check_names_in_expr(v, out);
            }
        }
        Stmt::Expr(s) => collect_nil_check_names_in_expr(&s.expr, out),
    }
}

fn collect_nil_check_names_in_expr(expr: &Expr, out: &mut std::collections::HashSet<String>) {
    match expr {
        Expr::StaticField(_) => {}
        Expr::FieldAccess(obj, name, _) => {
            out.insert(name.clone());
            collect_nil_check_names_in_expr(obj, out);
        }
        Expr::MethodCall(m) => {
            out.insert(m.method.clone());
            collect_nil_check_names_in_expr(&m.object, out);
            for arg in &m.args {
                collect_nil_check_names_in_expr(&arg.expr, out);
            }
        }
        Expr::Binary(b) => {
            collect_nil_check_names_in_expr(&b.lhs, out);
            collect_nil_check_names_in_expr(&b.rhs, out);
        }
        Expr::Unary(u) => collect_nil_check_names_in_expr(&u.expr, out),
        Expr::Call(c) => {
            for arg in &c.args {
                collect_nil_check_names_in_expr(&arg.expr, out);
            }
        }
        Expr::Ternary(t) => {
            collect_nil_check_names_in_expr(&t.condition, out);
            collect_nil_check_names_in_expr(&t.then_expr, out);
            collect_nil_check_names_in_expr(&t.else_expr, out);
        }
        Expr::Range(r) => {
            collect_nil_check_names_in_expr(&r.start, out);
            collect_nil_check_names_in_expr(&r.end, out);
        }
        Expr::Lambda(l) => match &l.body {
            LambdaBody::Expr(e) => collect_nil_check_names_in_expr(e, out),
            LambdaBody::Block(b) => collect_nil_check_names_in_block(b, out),
        },
        Expr::Print(e, _, _) => collect_nil_check_names_in_expr(e, out),
        Expr::Spawn(s) => {
            for arg in &s.args {
                collect_nil_check_names_in_expr(&arg.expr, out);
            }
        }
        Expr::Await(a) => collect_nil_check_names_in_expr(&a.expr, out),
        Expr::StaticCall(s) => {
            for arg in &s.args {
                collect_nil_check_names_in_expr(&arg.expr, out);
            }
        }
        Expr::ObjectLiteral(o) => {
            for f in &o.fields {
                collect_nil_check_names_in_expr(&f.value, out);
            }
        }
        Expr::Match(m) => {
            collect_nil_check_names_in_expr(&m.scrutinee, out);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_nil_check_names_in_expr(e, out),
                    MatchBody::Block(b) => collect_nil_check_names_in_block(b, out),
                }
            }
        }
        Expr::TryPropagate(inner, _) => collect_nil_check_names_in_expr(inner, out),
        Expr::ArrayLiteral(elements, _) => {
            for el in elements {
                collect_nil_check_names_in_expr(el, out);
            }
        }
        Expr::Index(arr, index, _) => {
            collect_nil_check_names_in_expr(arr, out);
            collect_nil_check_names_in_expr(index, out);
        }
        Expr::Select(s) => {
            for case in &s.cases {
                match &case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        collect_nil_check_names_in_expr(channel, out)
                    }
                    SelectCaseKind::Send { channel, value } => {
                        collect_nil_check_names_in_expr(channel, out);
                        collect_nil_check_names_in_expr(value, out);
                    }
                    SelectCaseKind::Default => {}
                }
                collect_nil_check_names_in_block(&case.body, out);
            }
        }
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::String(_, _)
        | Expr::Var(_, _) => {}
    }
}
