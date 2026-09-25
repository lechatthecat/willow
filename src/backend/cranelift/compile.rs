//! Top-level compilation and symbol-declaration methods for the Cranelift
//! backend (`compile_*` / `declare_*`, extracted from `mod.rs`). `compile_module`
//! / `compile_program` stay `pub` (the entry points); the rest are `pub(super)`.

use crate::diagnostics::Span;
use anyhow::Result;
use cranelift_codegen::ir::{AbiParam, InstBuilder, UserFuncName, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module};

use super::*;

/// The name of a lifted closure's hidden leading parameter — the environment
/// object it was called through (willow-0g8j.2.12).
///
/// `$` cannot appear in a source identifier, so this can never collide with a
/// parameter, a capture, or a local the program wrote.
pub(super) const CLOSURE_ENV_PARAM: &str = "$closure_env";

/// The hidden environment parameter a `closure`-typed lambda's lifted body
/// takes, or `None` for a `fn`-typed one, which is called through a bare code
/// address and has no environment at all (willow-0g8j.2.12).
///
/// Its declared type is the closure type itself: that is what the value IS, so
/// the parameter is rooted and traced like any other GC reference.
fn closure_env_param(lambda_type: Option<&Type>, span: Span) -> Option<Param> {
    match lambda_type {
        Some(ty @ Type::Closure(..)) => Some(Param {
            name: CLOSURE_ENV_PARAM.to_string(),
            ty: (ty.clone()).to_source(),
            mode: ParamMode::Value,
            span,
            type_span: span,
        }),
        _ => None,
    }
}

/// One emission target of a unit's body phase: the declaration the walker
/// needs, paired with the semantic identity its lowered IR is stored under.
///
/// Built by [`Codegen::module_body_plan`] / [`Codegen::program_body_plan`] and
/// emitted one at a time by [`Codegen::compile_body`], so no whole-unit IR map
/// or name re-keying sits between a unit's lowering and its emission
/// (willow-afb5.18).
pub struct UnitBody<'a> {
    /// `None` only on the standalone path, which has no session body index.
    body: Option<crate::parser::ast::BodyId>,
    target: BodyTarget<'a>,
}

enum BodyTarget<'a> {
    Lambda {
        name: &'a str,
        lambda: &'a LambdaExpr,
    },
    /// A free function, under the symbol this unit compiles it by: the bare
    /// name for the entry program, the mangled module symbol for a module.
    Function {
        symbol: std::borrow::Cow<'a, str>,
        decl: &'a FunctionDecl,
    },
    /// A method or a constructor's synthesized `init`, under the class decl the
    /// declaration phase built — qualified, for a module class.
    Method {
        class: &'a ClassDecl,
        method: std::borrow::Cow<'a, MethodDecl>,
    },
}

impl BodyTarget<'_> {
    fn kind(&self) -> &'static str {
        match self {
            BodyTarget::Lambda { .. } => "lambda",
            BodyTarget::Function { .. } => "function",
            BodyTarget::Method { .. } => "method",
        }
    }

    /// Whether the body index agrees with the plan about what this identity
    /// is. An interface default injected into a class is registered as that
    /// class's method, so it is a `Function` owner like any other method.
    fn accepts(&self, owner: crate::compiler_db::ids::BodyOwner) -> bool {
        use crate::compiler_db::ids::BodyOwner;
        match self {
            BodyTarget::Lambda { .. } => matches!(owner, BodyOwner::Lambda { .. }),
            BodyTarget::Function { .. } => matches!(
                owner,
                BodyOwner::Function(_) | BodyOwner::StaticInitializer(_)
            ),
            BodyTarget::Method { .. } => matches!(
                owner,
                BodyOwner::Function(_)
                    | BodyOwner::Constructor { .. }
                    | BodyOwner::InterfaceDefault(_)
            ),
        }
    }
}

/// A compilation unit whose symbols are declared but whose bodies are not yet
/// lowered — carrying the per-unit state [`Codegen::declare_module`] derived so
/// [`Codegen::compile_module_bodies`] can reinstall it later.
///
/// Declaration and body lowering are separate driver phases (willow-4zt8):
/// EVERY unit — each imported module plus the entry program — is declared
/// before any body is lowered, so `virtual_dispatch_candidates` sees the whole
/// program's class hierarchy. Compiling each module completely in turn made a
/// module body devirtualize an `open` call against only the classes declared so
/// far, silently ignoring an override the entry file had not yet contributed.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct DeclaredModule {
    init_unit: InitUnitId,
    mod_name: String,
    /// The module program after the std-collection and coop-suspension
    /// normalizations, i.e. exactly what the declaration phase read.
    program: Program,
    /// Checked payload types extended by cooperative ANF for its fresh nodes.
    /// The driver lowers this exact program with these types; original-node
    /// captures and enum/pattern resolutions still come from its source checker.
    normalized_expr_types: HashMap<ExprId, crate::parser::ast::Type>,
    module_prefix: String,
    module_classes: Vec<(String, ClassDecl)>,
    /// Lambdas collected and declared by this module's declaration phase, under
    /// module-qualified symbols (willow-9yhi). The body phase compiles these
    /// same symbols; the collector numbers from zero per unit, so the names
    /// cannot be re-derived later without renumbering.
    lambdas: Vec<(String, LambdaExpr)>,
    /// The semantic body of each entry in `lambdas`, same order, walked once by
    /// the declaration phase (willow-afb5.18). Empty on the standalone path.
    lambda_bodies: Vec<crate::parser::ast::BodyId>,
    source_file: String,
    builtin_module_aliases: HashMap<String, String>,
    /// The module access names this unit's own `import`s name, plus the module
    /// itself (willow-vtlr). Like `builtin_module_aliases`, a later unit's
    /// declaration phase overwrites the backend's copy, so each unit carries
    /// its own to reinstall before its bodies are lowered.
    visible_modules: HashSet<String>,
    /// This module's own single-item imports (willow-28h8). They bind a LOCAL
    /// name to another module's symbol, and two units can bind the same local
    /// name to different modules, so they are rebound before these bodies.
    item_imports: Vec<ItemBinding>,
    /// The module prefixes this unit writes that the tables are not keyed by
    /// (willow-kd1v), reinstalled before these bodies for the same reason the
    /// item imports are: another unit's phase runs in between and restores
    /// them.
    module_spellings: Vec<ModuleSpelling>,
    enum_aliases: Vec<(String, EnumInfo)>,
}

/// The entry program's counterpart to [`DeclaredModule`].
#[derive(serde::Serialize, serde::Deserialize)]
pub struct DeclaredProgram {
    program: Program,
    /// Checked payload types extended by cooperative ANF for its fresh nodes.
    /// The driver lowers this exact program with these types; original-node
    /// captures and enum/pattern resolutions still come from its source checker.
    normalized_expr_types: HashMap<ExprId, crate::parser::ast::Type>,
    /// Lambdas collected and declared by the declaration phase; the body phase
    /// compiles these same symbols, so they are not re-collected (the collector
    /// numbers them by traversal order).
    lambdas: Vec<(String, LambdaExpr)>,
    /// The semantic body of each entry in `lambdas`, same order (willow-afb5.18).
    lambda_bodies: Vec<crate::parser::ast::BodyId>,
    source_file: String,
    builtin_module_aliases: HashMap<String, String>,
    /// The module access names the entry file imports (willow-vtlr).
    visible_modules: HashSet<String>,
    /// The entry file's own single-item imports (willow-28h8), rebound before
    /// its bodies because a module's body phase runs in between and may have
    /// bound the same local name to its own module's symbol.
    item_imports: Vec<ItemBinding>,
    /// The module prefixes the entry file writes that the tables are not keyed
    /// by (willow-kd1v) — it aliased a module some other file had already
    /// registered under a different spelling.
    module_spellings: Vec<ModuleSpelling>,
    enum_aliases: Vec<(String, EnumInfo)>,
}

impl DeclaredModule {
    pub fn normalized_program(&self) -> &Program {
        &self.program
    }
    pub fn normalized_expr_types(&self) -> &HashMap<ExprId, crate::parser::ast::Type> {
        &self.normalized_expr_types
    }
}

impl DeclaredProgram {
    pub fn normalized_program(&self) -> &Program {
        &self.program
    }
    pub fn normalized_expr_types(&self) -> &HashMap<ExprId, crate::parser::ast::Type> {
        &self.normalized_expr_types
    }
}

pub use crate::compiler_db::scope::{ItemBinding, ModuleSpelling, UnitImports};

/// The type/layout environment the LIR eligibility walker and the LIR emitter
/// read, built from the compiler's own tables so a form is admitted exactly
/// when emission can produce it.
///
/// This is a macro rather than a method because every field is a reference to a
/// closure temporary: returning the struct from a function would drop those
/// closures at the return. Expanded in place, they live as long as the `let`
/// that binds the context (willow-0g8j.2.18).
macro_rules! lir_type_ctx {
    ($me:expr, $return_type:expr) => {
        super::lir_gen::LirTypeCtx {
            known_fn: &|n| $me.func_ids.contains_key(n),
            classes: &$me.classes(),
            is_interface: &|n| $me.classes().is_interface(n),
            iface_identity: &|n| $me.classes().interface(n).map(|i| i.name.clone()),
            can_box: &|class, iface| {
                super::emit::resolve_vtable_id(
                    &$me.vtable_ids,
                    &$me.classes(),
                    &class.to_string(),
                    &iface.to_string(),
                )
                .is_some()
            },
            // The same `enum_infos` table `enum_variant_tag` and
            // `enum_is_gc_object_type` answer from, so the tags and the
            // representation eligibility vets are the ones emission uses
            // (willow-0g8j.8).
            enum_def: &|n| {
                let info = $me.enum_infos.get(n)?;
                Some(super::lir_gen::LirEnumDef {
                    identity: info.name.clone(),
                    type_params: info.type_params.clone(),
                    variants: info
                        .variants
                        .iter()
                        .map(|v| super::lir_gen::LirEnumVariant {
                            name: v.name.clone(),
                            payloads: v.payload_types.clone(),
                        })
                        .collect(),
                })
            },
            // Straight from the table the vtables are emitted from, so the
            // slot the walker vets is the slot it will index.
            iface_method: &|iface_ty, method| {
                let (iface, args): (&TypeId, &[Type]) = match iface_ty {
                    Type::Named(name) => (name, &[]),
                    Type::Generic(name, args) => (name, args),
                    _ => return None,
                };
                let info = $me.classes().interface(iface)?;
                if info.type_params.len() != args.len() {
                    return None;
                }
                super::vtable_layout::slot_of(&$me.classes(), iface, method)?;
                let sig = info.methods.get(method)?;
                let mut substitutions: HashMap<TypeId, Type> = info
                    .type_params
                    .iter()
                    .cloned()
                    .zip(args.iter().cloned())
                    .collect();
                substitutions.insert(TypeId::local("Self"), iface_ty.clone());
                Some(super::lir_gen::IfaceMethodSig {
                    params: sig
                        .params
                        .iter()
                        .map(|ty| crate::semantic::symbols::substitute_type(ty, &substitutions))
                        .collect(),
                    modes: sig.param_infos.iter().map(|p| p.mode.clone()).collect(),
                    ret: crate::semantic::symbols::substitute_type(
                        &sig.return_type,
                        &substitutions,
                    ),
                })
            },
            // Resolved through the same hierarchy walk
            // `emit_static_field_read` uses, so an inherited static is
            // admitted iff the emitter can find its data slot.
            static_field: &|class, field| {
                super::lookup_static_storage_in(&$me.static_storage, &$me.classes(), class, field)
                    .map(|info| info.ty)
            },
            // The same layout `declare_one_vtable` emits from, so the
            // offset eligibility vets is the offset the widening adds.
            iface_widen_path: &|target, source| {
                super::vtable_layout::super_path(&$me.classes(), source, target)
            },
            fn_types: &$me.fn_types,
            func_param_modes: &$me.func_param_modes,
            known_modules: &$me.known_modules,
            // The same set emission resolves a bare module class name from
            // (willow-vtlr), so eligibility and emission cannot disagree about
            // which module a name means.
            visible_modules: &$me.visible_modules,
            builtin_module_aliases: &$me.builtin_module_aliases,
            #[cfg(test)]
            return_type: $return_type,
            // Per-FUNCTION, like `return_type`: `lir_rejection_reason` sets it
            // from the function it is vetting, so nothing here can get it
            // wrong (willow-0g8j.13).
            self_class: None,
            // The same table emission reads for a lambda's address, so the
            // symbol eligibility vets is the symbol emission takes the address
            // of (willow-0g8j.2.2).
            lambda_symbol: &|id| $me.lambda_names.get(&id).cloned(),
            cooperative_leaves: &$me.cooperative_leaves,
        }
    };
}

impl Codegen {
    /// Compile an imported module. Functions are given the mangled name
    /// `{canonical_module_path}__{fn}` with `::` normalized to `__`.
    /// Must be called before `compile_program` so the entry module can call them.
    ///
    /// Declares and compiles in one call. A multi-unit driver should instead
    /// call [`Codegen::declare_module`] for every unit first and only then
    /// [`Codegen::compile_module_bodies`] (willow-4zt8).
    pub fn compile_module(
        &mut self,
        mod_name: &str,
        canonical_path: &str,
        program: &Program,
        source_file: &str,
    ) -> Result<()> {
        let unit = self.declare_module(mod_name, canonical_path, program, source_file)?;
        self.compile_module_bodies(&unit)
    }

    /// Declare every symbol an imported module contributes — class layouts,
    /// methods, static storage, function signatures, vtables and descriptors —
    /// without lowering a single body.
    pub fn declare_module(
        &mut self,
        mod_name: &str,
        canonical_path: &str,
        program: &Program,
        source_file: &str,
    ) -> Result<DeclaredModule> {
        let types = std::mem::take(&mut self.expr_types);
        // The standalone entry point has no resolver classification; the
        // driver passes one through `*_with_types` instead.
        let scope = crate::compiler_db::scope::UnitScope::default();
        let result = self.declare_module_with_types(
            mod_name,
            canonical_path,
            program,
            source_file,
            &types,
            scope,
        );
        self.expr_types = types;
        result
    }

    pub(crate) fn declare_module_with_types(
        &mut self,
        mod_name: &str,
        canonical_path: &str,
        program: &Program,
        source_file: &str,
        expr_types: &HashMap<ExprId, Type>,
        scope: crate::compiler_db::scope::UnitScope,
    ) -> Result<DeclaredModule> {
        let init_unit = self.module_init_plan.ensure_module(canonical_path);
        // Recorded from the RAW program, because the normalization on the next
        // line is what erases the aliases from it (willow-nswv).
        self.builtin_module_aliases = builtin_module_aliases(program);
        // The imports the resolver classified for THIS file. A module sees what
        // it imports, plus itself: its own classes are keyed
        // `{mod_name}::{Class}` by the tables below (willow-vtlr).
        let crate::compiler_db::scope::UnitScope {
            imports: unit_imports,
            enum_aliases,
        } = scope;
        self.visible_modules = unit_imports.visible_modules;
        self.visible_modules.insert(mod_name.to_string());
        // The item half is not bound here: the modules this one imports are
        // declared, but a direct TYPE import aliases whole compiled tables and
        // doing that per module changes what the classes declared after it are
        // compiled against. Only the function half is rebound, before this
        // unit's bodies (willow-28h8).
        let item_imports = unit_imports.item_imports;
        let module_spellings = unit_imports.module_spellings;
        // The one type table that does have to answer under this unit's own
        // spelling while its declarations are made: `declare_vtables_for_classes`
        // below resolves each `implements` name in `interface_infos`, and an
        // interface this unit IMPORTED (`import proto::Describable;`) is keyed
        // there by its canonical `proto::Describable` alone. The lookup missed,
        // the vtable was silently skipped, and every later boxing site fell back
        // to the raw object (willow-0g8j.3). Installed under a snapshot and taken
        // back out below, so nothing declared after this unit sees it.
        self.with_unit_resolution(self.resolution_context(), |this| {
            this.bind_unit_enum_aliases(&enum_aliases);
            this.alias_item_import_types(&item_imports);
            let normalized_program = match &this.body_queries {
                Some(queries) => queries.normalized_program(program)?,
                None => normalize_std_collection_program(program),
            };
            let program = &normalized_program;
            this.source_file = source_file.to_string();
            let module_prefix = module_symbol_prefix(canonical_path);
            let InitUnitId::Module(module_id) = init_unit else {
                unreachable!("module declaration has a module identity");
            };
            this.known_modules
                .register(module_id, canonical_path, mod_name);
            // ...and the module prefixes THIS unit writes for modules the graph
            // registered under some other spelling (willow-kd1v). Every module this
            // one imports is already declared — dependencies are declared before
            // their dependents — so the tables the aliases point at exist.
            this.alias_unit_module_spellings(&module_spellings);
            this.declare_runtime()?;
            this.declare_string_literals(program)?;
            // The source path backs `PanicInfo.file` for every panic, so it is a
            // release-mode literal too: V1 records message AND source location
            // (willow-s9ej.7).
            this.declare_string_literal(source_file)?;
            if this.build_mode == BuildMode::Debug {
                for name in collect_nil_check_names(program) {
                    this.declare_string_literal(&name)?;
                }
                this.declare_reference_debug_strings(program)?;
            }

            // INTERFACE names declared in this module, so a module-local (possibly
            // generic) interface named in an `implements` / signature by its bare name
            // is qualified to `module::Iface` (qualify_module_type alone does not
            // qualify a generic head name).
            let local_type_names: std::collections::HashSet<String> = program
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::Interface(i) => Some(i.name.clone()),
                    _ => None,
                })
                .collect();
            // ENUM names declared in this module, qualified by the module's
            // CANONICAL path rather than the name this unit reaches it by: an enum
            // has one identity build-wide, and the type checker qualifies module
            // signatures the same way, so what the walker reads off a cross-module
            // call has to be the same name (willow-itcw).
            let local_enum_names: std::collections::HashSet<String> = program
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::Enum(e) => Some(e.name.clone()),
                    _ => None,
                })
                .collect();

            // CLASS names declared in this module, used to qualify a module-local
            // `extends Base` so the subclass's class_base / layout / inherited-method
            // resolution all key off `module::Base` (willow-2egr).
            let local_class_names: std::collections::HashSet<String> = program
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::Class(c) => Some(c.name.clone()),
                    _ => None,
                })
                .collect();

            // Function signatures must qualify module-local classes too. Otherwise
            // `one::make() -> Point` and `two::make() -> Point` collapse to the same
            // bare type even though their layouts have different identities. A
            // directly imported short name still compares equal through the shared
            // class type id (willow-0g8j.3).
            let mut local_signature_type_names = local_type_names.clone();
            local_signature_type_names.extend(local_class_names.iter().cloned());

            // Every type a module class mentions is qualified the same way this
            // unit's FUNCTION signatures are (below): module-local classes and
            // interfaces under the unit's own spelling, module-local enums under the
            // build-wide canonical path. A name that is neither -- a type the module
            // itself imported (`import proto::Grade;`), or a builtin -- is left
            // exactly as written, so it still resolves to the one table that answers
            // to it. Prefixing wholesale renamed such a type into `lib::Grade`, which
            // nothing declares (willow-sxcp).
            // ...and a name that is neither is resolved through the aliases this
            // unit's own item imports installed above, so what leaves the module is
            // the identity the build's tables answer to (`Sized` -> `proto::Sized`).
            // A bare class name a later unit can still find by scanning modules
            // (`resolve_class_key`), but an interface has no such scan: left bare in
            // an exported signature, it made every box site in a consumer that did
            // not itself import the interface fall out of the walker's subset.
            let qualify_class_type = |ty: &crate::parser::ast::Type| -> crate::parser::ast::Type {
                let ty = qualify_module_local_type(ty, mod_name, &local_signature_type_names);
                let ty = qualify_module_local_type(&ty, canonical_path, &local_enum_names);
                this.canonical_declared_type(&ty).to_source()
            };
            let module_classes: Vec<(String, ClassDecl)> = program
                .items
                .iter()
                .filter_map(|item| {
                    let Item::Class(c) = item else {
                        return None;
                    };
                    let local_name = c.name.clone();
                    // A module-local `implements` name (generic included --
                    // `implements Box<i64>` -> `boxmod2::Box<i64>`) is qualified so
                    // its vtable is declared and keyed by the same name the entry
                    // boxes against (willow-1js.5), while an interface this module
                    // merely IMPORTED keeps the spelling it was written with:
                    // renaming `import proto::Describable;` into `impls::Describable`
                    // made the vtable lookup below silently find no interface, so the
                    // class got NO vtable and every later box site fell back to the
                    // raw object (willow-0g8j.3).
                    let mut qualified = qualify_module_class_decl(c, mod_name, &qualify_class_type);
                    // Qualify a module-local base class so `name()` yields the
                    // module-qualified base (TypePath::name() returns only the last
                    // segment, so the qualified name must live in a single Local
                    // string) (willow-2egr).
                    let module_local_base = match &qualified.base_class {
                        Some(TypePath::Local(name)) if local_class_names.contains(name) => {
                            Some(name.clone())
                        }
                        _ => None,
                    };
                    if let Some(base) = module_local_base {
                        qualified.base_class = Some(TypePath::Local(format!("{mod_name}::{base}")));
                    }
                    Some((local_name, qualified))
                })
                .collect();

            // Register imported module class layouts and methods under their
            // module-qualified names so entry code can call `geom::Point::new(...)`.
            //
            // Registration only records each class's OWN fields and virtual
            // methods; the inherited ones are prepended afterwards by
            // `finalize_class_layouts`, which evaluates the `extends` chain
            // root-down. A cross-module hierarchy can arrive subclass-first too,
            // and no declaration order may change a layout (willow-59gx). The
            // same evaluation settles each class's virtual slot ORDER, which the
            // vtables below need: an `open` method's vtable slot holds a thunk
            // that dispatches through virtual slot N of the receiver's
            // descriptor (willow-tygf).
            for (_, c) in &module_classes {
                this.register_class_layout(c)?;
            }
            this.finalize_class_layouts()?;
            for (_, c) in &module_classes {
                this.declare_class_methods(c)?;
                // Static-property storage for imported modules (replayed by
                // `__willow_static_init`, compiled in the entry's compile_program).
                this.declare_static_storage_for_class(&c.name, c, init_unit)?;
            }

            // Forward-declare all functions in this module. The declaration records
            // the SIGNATURE-qualified type metadata (fn_types / param debug); the body
            // is compiled later from the original `f` under local-name aliases.
            for item in &program.items {
                match item {
                    Item::Function(f) => {
                        let mangled = module_item_symbol(&module_prefix, &f.name);
                        let qualified =
                            qualify_module_fn_signature(f, mod_name, &local_signature_type_names);
                        let mut qualified = qualify_module_fn_signature(
                            &qualified,
                            canonical_path,
                            &local_enum_names,
                        );
                        // Same translation the classes above get: a type this module
                        // reached through its own item import is exported under the
                        // identity the tables hold, not the bare local spelling
                        // (willow-sxcp).
                        for param in &mut qualified.params {
                            param.ty = (this.canonical_declared_type(&param.ty)).to_source();
                        }
                        qualified.return_type =
                            (this.canonical_declared_type(&qualified.return_type)).to_source();
                        this.func_ids.scope().declare(
                            &mangled,
                            FunctionId::free(&f.name).in_namespace(canonical_path),
                        );
                        this.declare_function_named(&mangled, &qualified)?;
                    }
                    Item::Enum(_) | Item::Class(_) | Item::Interface(_) => {}
                }
            }

            // Emit (class, interface) vtables for module classes that implement an
            // interface (their methods are declared above; implements paths were
            // module-qualified by `qualify_module_class_decl`).
            let qualified_classes: Vec<ClassDecl> =
                module_classes.iter().map(|(_, c)| c.clone()).collect();
            // Every ancestor of a class in this unit is already registered —
            // dependencies are declared before their dependents — so its slot
            // order was final when the layouts were completed above.
            this.declare_vtables_for_classes(&qualified_classes)?;

            // Emit one descriptor per module class: word 0 of every object of that
            // class, holding its `type_id` and its virtual method slots
            // (willow-fm7t). Must follow the method declarations above, since every
            // slot is filled by function address.
            this.declare_class_descriptors_for(&qualified_classes)?;

            // Collect and declare this module's lambdas, exactly as
            // `declare_program` does for the entry file (willow-9yhi). Lifting a
            // lambda is a DECLARATION-phase job: `lambda_names` is what both
            // emitters read to find the lifted symbol for a lambda expression, so
            // without this pass a module body's lambda reaches codegen with no name
            // at all. The symbols carry the module prefix because the collector
            // restarts its numbering for every unit it is given.
            let lambdas: Vec<(String, LambdaExpr)> = collect_lambdas_in_program(program)
                .into_iter()
                .enumerate()
                .map(|(index, (_, lambda))| (module_lambda_symbol(&module_prefix, index), lambda))
                .collect();
            for (index, (name, lambda)) in lambdas.iter().enumerate() {
                this.func_ids.scope().declare(
                    name,
                    FunctionId::free(lambda_symbol(index)).in_namespace(canonical_path),
                );
                this.declare_lambda(name, lambda, expr_types)?;
                this.lambda_names
                    .insert(lambda.id, this.func_ids.scope().lookup_id(name));
            }

            // Analyze under canonical backend names before installing the module's
            // temporary local aliases. Imported/unknown callees remain conservative
            // unless an earlier module already published an explicit summary.
            let lambda_bodies = this.unit_lambda_bodies(program, &lambdas)?;
            this.bind_lambda_body_names(&lambda_bodies, &lambdas);
            this.analyze_and_register_panic_effects(
                program,
                super::panic_effect::UnitNaming {
                    module_prefix: Some(&module_prefix),
                },
                &lambdas,
                &lambda_bodies,
                expr_types,
            )?;

            Ok(DeclaredModule {
                init_unit,
                mod_name: mod_name.to_string(),
                program: normalized_program,
                normalized_expr_types: expr_types
                    .iter()
                    .map(|(id, ty)| (*id, ty.to_source()))
                    .collect(),
                module_prefix,
                module_classes,
                lambdas,
                lambda_bodies,
                source_file: source_file.to_string(),
                builtin_module_aliases: std::mem::take(&mut this.builtin_module_aliases),
                visible_modules: std::mem::take(&mut this.visible_modules),
                item_imports,
                module_spellings,
                enum_aliases,
            })
        })
    }

    /// The ordered emission targets of a module's body phase, each paired with
    /// the semantic body its lowered IR is stored under (willow-afb5.18).
    ///
    /// Built once per unit, in the order the bodies are emitted: the lifted
    /// lambdas this module declared, its free functions, then each declared
    /// class's methods and constructors. A method of a class this module does
    /// not declare has no qualified decl to be compiled under, so it is not a
    /// target here — the same bodies the old name re-keying dropped.
    ///
    /// `body` is `None` on the standalone path, which has no body index and
    /// reads its IR out of `lir_functions` by name instead.
    pub fn module_body_plan<'a>(&self, unit: &'a DeclaredModule) -> Vec<UnitBody<'a>> {
        let mut plan = Vec::new();
        self.push_lambda_targets(&unit.lambdas, &unit.lambda_bodies, &mut plan);
        for item in &unit.program.items {
            if let Item::Function(f) = item {
                plan.push(UnitBody {
                    body: self.semantic_body(f.body.id),
                    target: BodyTarget::Function {
                        symbol: module_item_symbol(&unit.module_prefix, &f.name).into(),
                        decl: f,
                    },
                });
            }
        }
        for (_, class) in &unit.module_classes {
            self.push_class_targets(class, &mut plan);
        }
        plan
    }

    /// The entry program's counterpart to [`Codegen::module_body_plan`].
    /// Functions and classes are interleaved in item order, as the entry's
    /// body phase has always emitted them.
    pub fn program_body_plan<'a>(&self, unit: &'a DeclaredProgram) -> Vec<UnitBody<'a>> {
        let mut plan = Vec::new();
        self.push_lambda_targets(&unit.lambdas, &unit.lambda_bodies, &mut plan);
        for item in &unit.program.items {
            match item {
                Item::Function(f) => plan.push(UnitBody {
                    body: self.semantic_body(f.body.id),
                    target: BodyTarget::Function {
                        symbol: f.name.as_str().into(),
                        decl: f,
                    },
                }),
                Item::Class(c) => self.push_class_targets(c, &mut plan),
                Item::Enum(_) | Item::Interface(_) => {}
            }
        }
        plan
    }

    /// A lifted lambda's body identity comes from the declaration inventory,
    /// not from the synthesized `FunctionDecl` the emitter builds for it: the
    /// two orders are the same walk, which `bind_lambda_body_names` already
    /// relies on.
    fn push_lambda_targets<'a>(
        &self,
        lambdas: &'a [(String, LambdaExpr)],
        bodies: &[crate::parser::ast::BodyId],
        plan: &mut Vec<UnitBody<'a>>,
    ) {
        for (index, (name, lambda)) in lambdas.iter().enumerate() {
            plan.push(UnitBody {
                body: bodies.get(index).copied(),
                target: BodyTarget::Lambda { name, lambda },
            });
        }
    }

    fn push_class_targets<'a>(&self, class: &'a ClassDecl, plan: &mut Vec<UnitBody<'a>>) {
        for method in &class.methods {
            plan.push(UnitBody {
                body: self.semantic_body(method.body.id),
                target: BodyTarget::Method {
                    class,
                    method: std::borrow::Cow::Borrowed(method),
                },
            });
        }
        // Each constructor is emitted as its synthesized `init` method
        // (willow-scq2); the synthesized shell clones the constructor block, so
        // it carries the constructor's own body identity.
        for ctor in &class.constructors {
            plan.push(UnitBody {
                body: self.semantic_body(ctor.body.id),
                target: BodyTarget::Method {
                    class,
                    method: std::borrow::Cow::Owned(constructor_to_method(ctor)),
                },
            });
        }
    }

    /// The semantic body of a static property's initializer expression.
    ///
    /// Unlike a function or method, the emitter compiles a synthesized shell
    /// whose block has a fresh id, so the identity has to come from the index's
    /// static-initializer registration instead of from the AST node.
    fn static_initializer_body(
        &self,
        initializer: &Expr,
    ) -> Result<Option<crate::parser::ast::BodyId>> {
        let Some(queries) = &self.body_queries else {
            return Ok(None);
        };
        let index = queries.index();
        let static_id = index
            .static_id(initializer.id())
            .ok_or_else(|| anyhow::anyhow!("static initializer has no identity"))?;
        let body = index
            .body(
                static_id.unit,
                crate::compiler_db::ids::BodyOwner::StaticInitializer(static_id),
            )
            .ok_or_else(|| anyhow::anyhow!("static initializer has no body identity"))?;
        Ok(Some(body))
    }

    /// The body identity emission keys off, or `None` when this backend has no
    /// session body index (the standalone path).
    fn semantic_body(
        &self,
        body: crate::parser::ast::BodyId,
    ) -> Option<crate::parser::ast::BodyId> {
        self.body_queries.as_ref().map(|_| body)
    }

    /// Emit one body of the unit whose alias scope is installed.
    ///
    /// The lowered IR is read from `lir_body(BodyId)` — one artifact, for this
    /// body alone — so no whole-unit IR map stands between lowering and
    /// emission. The body index answers what kind of declaration the identity
    /// belongs to, which is checked against the target the plan recorded.
    pub fn compile_body(&mut self, target: &UnitBody<'_>) -> Result<()> {
        if let (Some(body), Some(queries)) = (target.body, &self.body_queries) {
            let (_, owner) = queries
                .index()
                .owner(body)
                .ok_or_else(|| anyhow::anyhow!("emission target {body:?} has no body identity"))?;
            anyhow::ensure!(
                target.target.accepts(owner),
                "body {body:?} ({owner:?}) is not a {} emission target",
                target.target.kind()
            );
        }
        match &target.target {
            BodyTarget::Lambda { name, lambda } => self.compile_lambda(name, lambda, target.body),
            BodyTarget::Function { symbol, decl } => {
                self.compile_function_named(symbol.as_ref(), decl, target.body)
            }
            BodyTarget::Method { class, method } => {
                self.compile_class_method(class, method.as_ref(), target.body)
            }
        }
    }

    /// Install a module's body-phase view of the build — its aliases, imports
    /// and source path — run `emit` under it, then compile that unit's static
    /// initializers and restore the previous view.
    ///
    /// The declaration phase of a LATER unit has overwritten all three of the
    /// installed tables, so this module's own view has to be reinstalled before
    /// its bodies are lowered (willow-4zt8). An item import binds a LOCAL name
    /// globally, so the last unit to bind it owns it; this module's bodies have
    /// to call the module IT imported (willow-28h8).
    pub fn with_module_bodies(
        &mut self,
        unit: &DeclaredModule,
        emit: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        self.builtin_module_aliases = unit.builtin_module_aliases.clone();
        self.visible_modules = unit.visible_modules.clone();
        self.source_file = unit.source_file.clone();
        let program = &unit.program;

        let result = self.with_unit_resolution(self.resolution_context(), |this| {
            this.bind_unit_enum_aliases(&unit.enum_aliases);
            this.rebind_item_import_functions(&unit.item_imports);
            // Bind the types this unit imported by single-item import under the
            // local names it spells them by (willow-0g8j.3), then the module's own
            // enums/interfaces under their unqualified names so the module body
            // resolves its own types internally (willow-64gs.1). Own declarations
            // are installed second, so they win.
            this.alias_item_import_types(&unit.item_imports);
            // Before the unit's own names, so a module reached under two spellings
            // still loses to a type this module declares itself (willow-kd1v).
            this.alias_unit_module_spellings(&unit.module_spellings);
            this.alias_module_local_types(program, &unit.mod_name);
            for item in &program.items {
                if let Item::Function(f) = item {
                    let mangled = module_item_symbol(&unit.module_prefix, &f.name);
                    this.alias_function_symbol(&f.name, &mangled);
                }
            }
            for (local_name, qualified) in &unit.module_classes {
                this.alias_class_symbol(local_name, &qualified.name);
                if !qualified.constructors.is_empty() {
                    let local = class_member_symbol(local_name, "init");
                    let canonical = this.class_method_symbol(&qualified.name, "init");
                    this.alias_function_symbol(&local, &canonical);
                }
                for method in &qualified.methods {
                    let local_mangled = class_member_symbol(local_name, &method.name);
                    let qualified_mangled = this.class_method_symbol(&qualified.name, &method.name);
                    this.alias_function_symbol(&local_mangled, &qualified_mangled);
                }
            }

            (|| -> Result<()> {
                emit(this)?;
                // Inside the alias scope, for the reason the bodies are.
                this.compile_unit_static_init(unit)
            })()
        });
        self.release_unit_transients();
        result
    }

    /// Lower the bodies of a module already passed through
    /// [`Codegen::declare_module`], under that module's temporary unqualified
    /// aliases. The session driver iterates
    /// [`Codegen::module_body_plan`] instead, one body at a time.
    pub fn compile_module_bodies(&mut self, unit: &DeclaredModule) -> Result<()> {
        let plan = self.module_body_plan(unit);
        self.with_module_bodies(unit, |this| {
            for target in &plan {
                this.compile_body(target)?;
            }
            Ok(())
        })
    }

    /// Declare and compile the entry program in one call. A multi-unit driver
    /// should instead run [`Codegen::declare_program`] alongside every module's
    /// [`Codegen::declare_module`] and only then lower any body (willow-4zt8).
    pub fn compile_program(&mut self, program: &Program, source_file: &str) -> Result<()> {
        let unit = self.declare_program(program, source_file)?;
        self.compile_program_bodies(&unit)
    }

    /// Declare every symbol the entry program contributes, without lowering a
    /// single body.
    pub fn declare_program(
        &mut self,
        program: &Program,
        source_file: &str,
    ) -> Result<DeclaredProgram> {
        let types = std::mem::take(&mut self.expr_types);
        // The standalone entry point has no resolver classification; the
        // driver passes one through `*_with_types` instead.
        let scope = crate::compiler_db::scope::UnitScope::default();
        let result = self.declare_program_with_types(program, source_file, &types, scope);
        self.expr_types = types;
        result
    }

    pub(crate) fn declare_program_with_types(
        &mut self,
        program: &Program,
        source_file: &str,
        expr_types: &HashMap<ExprId, Type>,
        scope: crate::compiler_db::scope::UnitScope,
    ) -> Result<DeclaredProgram> {
        // Recorded from the RAW program, because the normalization on the next
        // line is what erases the aliases from it (willow-nswv).
        self.builtin_module_aliases = builtin_module_aliases(program);
        // The entry file's own classified imports (willow-vtlr, willow-28h8).
        // Every module is declared by now, so the entry's single-item imports
        // bind here, function and directly imported type alike — the entry is
        // the last unit declared, so nothing is compiled against these tables
        // before they are aliased.
        let crate::compiler_db::scope::UnitScope {
            imports: unit_imports,
            enum_aliases,
        } = scope;
        self.visible_modules = unit_imports.visible_modules;
        let item_imports = unit_imports.item_imports;
        let module_spellings = unit_imports.module_spellings;
        for item in &item_imports {
            self.register_item_import(&item.local, &item.module, &item.item);
        }
        // The entry's own spellings for modules another file registered under a
        // different name (willow-kd1v). Snapshotted and taken back out below:
        // every module's BODY phase runs between this declaration and the
        // entry's own, and those units spell the same modules their own way.
        self.with_unit_resolution(self.resolution_context(), |this| {
            this.bind_unit_enum_aliases(&enum_aliases);
            this.alias_unit_module_spellings(&module_spellings);
            let normalized_program = match &this.body_queries {
                Some(queries) => queries.normalized_program(program)?,
                None => normalize_std_collection_program(program),
            };
            let program = &normalized_program;
            this.source_file = source_file.to_string();
            this.declare_runtime()?;
            this.declare_string_literals(program)?;
            // The source path backs `PanicInfo.file` for every panic, so it is a
            // release-mode literal too: V1 records message AND source location
            // (willow-s9ej.7).
            this.declare_string_literal(source_file)?;
            if this.build_mode == BuildMode::Debug {
                for name in collect_nil_check_names(program) {
                    this.declare_string_literal(&name)?;
                }
                this.declare_reference_debug_strings(program)?;
            }

            // Pass 1: record every class's OWN fields, base and type_id. No
            // inherited field is resolved here, because a subclass may be declared
            // before its base and reading a base mid-walk sees whatever has been
            // registered so far (willow-59gx).
            for item in &program.items {
                if let Item::Class(c) = item {
                    this.register_class_layout(c)?;
                }
            }
            // Pass 2: prepend inherited fields by evaluating each `extends` chain
            // root-down, which makes the result independent of declaration order.
            // The same pass settles every class's virtual slot ORDER, which the
            // vtables below need: an `open` method's vtable slot holds a thunk
            // that dispatches through virtual slot N of the receiver's
            // descriptor (willow-tygf).
            this.finalize_class_layouts()?;
            // Pass 3: forward-declare methods and static storage, now that every
            // layout is final -- a constructor's parameter order IS the layout.
            for item in &program.items {
                match item {
                    Item::Class(c) => {
                        this.declare_class_methods(c)?;
                        this.declare_static_storage_for_class(&c.name, c, InitUnitId::Entry)?;
                    }
                    Item::Enum(_) => {} // enum infos are registered via register_enum_info before compile
                    _ => {}
                }
            }

            // Forward-declare all user functions first
            for item in &program.items {
                match item {
                    Item::Function(f) => this.declare_user_function(f)?,
                    Item::Class(_) | Item::Enum(_) | Item::Interface(_) => {}
                }
            }

            // Emit one static vtable per (class, implemented-interface) pair. All
            // class method symbols are declared by now, so the vtable can reference
            // them by function address.
            this.declare_interface_vtables(program)?;

            // Emit one descriptor per class (willow-fm7t). Unlike interface
            // vtables this covers EVERY class, since word 0 of every object points
            // at its descriptor whether or not the class implements an interface.
            this.declare_class_descriptors(program)?;

            // Collect and declare all lambdas (they may call user functions already declared above).
            let lambdas = collect_lambdas_in_program(program);
            for (name, lambda) in &lambdas {
                this.declare_lambda(name, lambda, expr_types)?;
                this.lambda_names
                    .insert(lambda.id, this.func_ids.scope().lookup_id(name));
                // The lowered body was lifted under a span-derived placeholder
                // because only this loop knows the symbol (willow-0g8j.2.2). Moving
                // it into `lir_functions` under that symbol is what lets a lambda be
                // compiled by the walker like any other function.
                if let Some(mut lf) = this.lir_lambdas.remove(&lambda.id) {
                    lf.name = this.func_ids.scope().lookup_id(name);
                    this.lir_functions.insert(lf.name, lf);
                }
            }

            let lambda_bodies = this.unit_lambda_bodies(program, &lambdas)?;
            this.bind_lambda_body_names(&lambda_bodies, &lambdas);
            this.analyze_and_register_panic_effects(
                program,
                super::panic_effect::UnitNaming {
                    module_prefix: None,
                },
                &lambdas,
                &lambda_bodies,
                expr_types,
            )?;

            // Always declare `__willow_static_init` (willow-qsqf §13.5). The runtime
            // calls it after `gc_init` and before `willow_user_main`; it is a no-op
            // when the program has no static properties. Declaring it unconditionally
            // keeps the runtime call path uniform regardless of the `main` lowering.
            this.declare_static_init()?;

            Ok(DeclaredProgram {
                program: normalized_program,
                normalized_expr_types: expr_types
                    .iter()
                    .map(|(id, ty)| (*id, ty.to_source()))
                    .collect(),
                lambdas,
                lambda_bodies,
                source_file: source_file.to_string(),
                builtin_module_aliases: std::mem::take(&mut this.builtin_module_aliases),
                visible_modules: std::mem::take(&mut this.visible_modules),
                item_imports,
                module_spellings,
                enum_aliases,
            })
        })
    }

    /// Install the entry program's body-phase view of the build, run `emit`
    /// under it, then compile `__willow_static_init` and restore the previous
    /// view.
    ///
    /// A module's body phase runs between the two entry phases and installs its
    /// own view of both (willow-4zt8), including the entry's own module
    /// spellings, which the last module body phase took back out (willow-kd1v).
    pub fn with_program_bodies(
        &mut self,
        unit: &DeclaredProgram,
        emit: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        self.builtin_module_aliases = unit.builtin_module_aliases.clone();
        self.visible_modules = unit.visible_modules.clone();
        self.source_file = unit.source_file.clone();
        let result = self.with_unit_resolution(self.resolution_context(), |this| {
            this.bind_unit_enum_aliases(&unit.enum_aliases);
            this.rebind_item_import_functions(&unit.item_imports);
            // Declarations no longer leak their unit aliases into later body
            // phases. Install this entry unit's type imports explicitly too.
            this.alias_item_import_types(&unit.item_imports);
            this.alias_unit_module_spellings(&unit.module_spellings);

            (|| -> Result<()> {
                emit(this)?;
                // Compile the static-init function body after all symbols are defined.
                this.compile_static_init()
            })()
        });
        self.release_unit_transients();
        result
    }

    /// Lower the bodies of an entry program already passed through
    /// [`Codegen::declare_program`], plus every lambda and the static
    /// initializer. The session driver iterates
    /// [`Codegen::program_body_plan`] instead, one body at a time.
    pub fn compile_program_bodies(&mut self, unit: &DeclaredProgram) -> Result<()> {
        let plan = self.program_body_plan(unit);
        self.with_program_bodies(unit, |this| {
            for target in &plan {
                this.compile_body(target)?;
            }
            Ok(())
        })
    }

    /// Declare the `__willow_static_init` symbol (no params, no returns). Exported
    /// so the runtime entry can call it before `main` (willow-qsqf §13.5).
    pub(super) fn declare_static_init(&mut self) -> Result<()> {
        if self.func_ids.contains_key(STATIC_INIT_SYMBOL) {
            return Ok(());
        }
        let sig = self.module.make_signature();
        let id = self
            .module
            .declare_function(STATIC_INIT_SYMBOL, Linkage::Export, &sig)?;
        self.func_ids.insert(STATIC_INIT_SYMBOL, id);
        Ok(())
    }

    /// Declare the signature for a lambda private function.
    pub(super) fn declare_lambda(
        &mut self,
        name: &str,
        l: &LambdaExpr,
        expr_types: &HashMap<ExprId, Type>,
    ) -> Result<()> {
        let lambda_type = Self::lambda_value_type(l, expr_types);
        let (param_types, ast_ret) = match &lambda_type {
            Some(Type::Fn(params, ret) | Type::Closure(params, ret)) => {
                (params.clone(), *ret.clone())
            }
            _ => anyhow::bail!(
                "lambda `{name}` at {:?} has no checked callable type",
                l.span
            ),
        };
        // A closure's lifted body takes its environment object as a hidden
        // LEADING argument (willow-0g8j.2.12). It is declared here, in the
        // signature and in every per-parameter table, so the index a caller
        // counts from and the index the body binds from stay the same one.
        let env_param = closure_env_param(lambda_type.as_ref(), l.span);
        let mut sig = self.module.make_signature();
        if env_param.is_some() {
            sig.params
                .push(AbiParam::new(reference_type(self.module.target_config())));
        }
        for ty in &param_types {
            sig.params.push(AbiParam::new(clif_type(
                reference_type(self.module.target_config()),
                ty,
            )));
        }
        sig.returns.push(AbiParam::new(clif_type(
            reference_type(self.module.target_config()),
            &ast_ret,
        )));
        let id = self.module.declare_function(name, Linkage::Local, &sig)?;
        self.func_ids.insert(name, id);
        self.func_return_types.insert(name, ast_ret.clone());
        self.func_param_modes.insert(
            name,
            env_param
                .iter()
                .map(|_| ParamMode::Value)
                .chain(l.params.iter().map(|_| ParamMode::Value))
                .collect(),
        );
        self.func_param_debug.insert(
            name,
            env_param
                .iter()
                .map(|p| ParamDebug {
                    name: p.name.clone(),
                    ty: p.ty.clone().into(),
                    mode: ParamMode::Value,
                })
                .chain(l.params.iter().zip(param_types.iter()).map(|p| ParamDebug {
                    name: p.0.name.clone(),
                    ty: p.1.clone(),
                    mode: ParamMode::Value,
                }))
                .collect(),
        );
        // The VALUE type, without the hidden environment parameter: this is
        // what an expression of this lambda's type is, and eligibility compares
        // the two directly.
        self.fn_types.insert(
            name,
            match lambda_type {
                Some(Type::Closure(..)) => Type::Closure(param_types, Box::new(ast_ret)),
                _ => Type::Fn(param_types, Box::new(ast_ret)),
            },
        );
        Ok(())
    }

    /// The checker's callable type for a lambda expression, if it recorded one.
    fn lambda_value_type(l: &LambdaExpr, expr_types: &HashMap<ExprId, Type>) -> Option<Type> {
        match expr_types.get(&l.id) {
            Some(ty @ (Type::Fn(..) | Type::Closure(..))) => Some(ty.clone()),
            _ => None,
        }
    }

    /// Compile a lambda as a private function.
    pub(super) fn compile_lambda(
        &mut self,
        name: &str,
        l: &LambdaExpr,
        semantic_body: Option<crate::parser::ast::BodyId>,
    ) -> Result<()> {
        let lambda_type = self.fn_types.get(name).cloned();
        let (param_types, return_type) = match &lambda_type {
            Some(Type::Fn(params, ret) | Type::Closure(params, ret)) => {
                (params.clone(), *ret.clone())
            }
            _ => anyhow::bail!(
                "lambda `{name}` at {:?} has no checked callable type",
                l.span
            ),
        };
        // Same leading environment parameter `declare_lambda` put in the
        // signature (willow-0g8j.2.12); the two are built from the same
        // checker type, so they cannot disagree.
        let params: Vec<Param> = closure_env_param(lambda_type.as_ref(), l.span)
            .into_iter()
            .chain(
                l.params
                    .iter()
                    .zip(param_types.iter())
                    .map(|(p, ty)| Param {
                        name: p.name.clone(),
                        ty: (ty.clone()).to_source(),
                        mode: ParamMode::Value,
                        span: p.span,
                        type_span: p.span,
                    }),
            )
            .collect();
        let body = match &l.body {
            LambdaBody::Block(b) => b.clone(),
            // A `void` body is a STATEMENT, not a returned value: synthesising
            // `return println(x);` would emit a `return` with an operand
            // against a signature that has no result slot (willow-0g8j.2.2).
            LambdaBody::Expr(e) if return_type == Type::Void => Block {
                id: crate::parser::ast::BodyId::fresh(),
                stmts: vec![Stmt::Expr(ExprStmt {
                    expr: *e.clone(),
                    span: e.span(),
                })],
                span: l.span,
            },
            LambdaBody::Expr(e) => Block {
                id: crate::parser::ast::BodyId::fresh(),
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
            return_type: return_type.to_source(),
            body,
            span: l.span,
        };
        self.compile_function_named(name, &f, semantic_body)
    }

    pub(super) fn declare_runtime(&mut self) -> Result<()> {
        if self.runtime_declared {
            return Ok(());
        }

        // The runtime ABI surface is declared from a single source of truth in
        // `willow_abi::runtime_symbols`. Adding or changing a runtime symbol
        // means editing `RUNTIME_SYMBOLS`, not this loop.
        let ptr_ty = reference_type(self.module.target_config());
        for symbol in abi::RUNTIME_SYMBOLS {
            let mut sig = self.module.make_signature();
            abi::fill_signature(symbol, &mut sig, ptr_ty);
            let id = self
                .module
                .declare_function(symbol.name, Linkage::Import, &sig)?;
            self.func_ids.insert(symbol.name, id);
        }
        self.runtime_declared = true;
        Ok(())
    }

    pub(super) fn declare_string_literals(&mut self, program: &Program) -> Result<()> {
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

    pub(super) fn declare_reference_debug_strings(&mut self, program: &Program) -> Result<()> {
        for value in collect_reference_debug_strings_in_program(program) {
            self.declare_string_literal(&value)?;
        }
        Ok(())
    }

    pub(super) fn declare_string_literal(&mut self, value: &str) -> Result<()> {
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
        // Writable, pointer-aligned, zero-initialized atomic slot. Its address is
        // process-lifetime storage; runtime reset clears initialized slots.
        let slot =
            self.module
                .declare_data(&format!("{name}_slot"), Linkage::Local, true, false)?;
        let mut slot_data = DataDescription::new();
        let pointer_bytes = self.module.target_config().pointer_bytes();
        slot_data.define_zeroinit(pointer_bytes as usize);
        slot_data.set_align(pointer_bytes as u64);
        self.module.define_data(slot, &slot_data)?;
        self.string_literals.insert(
            value.to_string(),
            StringLiteralData {
                bytes: data_id,
                slot,
            },
        );
        Ok(())
    }

    pub(super) fn declare_user_function(&mut self, f: &FunctionDecl) -> Result<()> {
        let symbol_name = user_function_symbol(&f.name);
        self.declare_function_symbol(&f.name, &symbol_name, f, f.name == "main")
    }

    pub(super) fn declare_function_named(&mut self, name: &str, f: &FunctionDecl) -> Result<()> {
        self.declare_function_symbol(name, name, f, false)
    }

    pub(super) fn declare_function_symbol(
        &mut self,
        lookup_name: &str,
        symbol_name: &str,
        f: &FunctionDecl,
        export: bool,
    ) -> Result<()> {
        *self.dispatch_cache.get_mut() = Default::default();
        let mut sig = self.module.make_signature();
        let ptr_ty = reference_type(self.module.target_config());
        // `willow_user_main` is parameterless even when `fn main(args:
        // Array<String>)` is declared (see compile_function_named).
        if symbol_name != USER_MAIN_SYMBOL {
            for param in &f.params {
                sig.params
                    .push(AbiParam::new(param_abi_type(param, ptr_ty)));
            }
        }
        let call_return_type = self.canonical_enum_type(&function_call_return_type(f));
        // A `Result<void, E>` main lowers to a VOID `willow_user_main` (it
        // inspects its result and exits in the body; willow-exg). Keep this in
        // sync with compile_function_named.
        let force_void_main = symbol_name == USER_MAIN_SYMBOL && main_result_err_type(f).is_some();
        if call_return_type != Type::Void && !force_void_main {
            sig.returns.push(AbiParam::new(clif_type(
                reference_type(self.module.target_config()),
                &call_return_type,
            )));
        }
        let linkage = if export {
            Linkage::Export
        } else {
            Linkage::Local
        };
        self.claim_symbol(symbol_name, format!("function `{}`", f.name), f.span)?;
        let id = self.module.declare_function(symbol_name, linkage, &sig)?;
        self.func_ids.insert(lookup_name, id);
        // Task constructors are global declarations. Unit aliases resolve to
        // these canonical identities, including calls within another module.
        if f.is_async && symbol_name != USER_MAIN_SYMBOL {
            self.cooperative_leaves
                .insert(self.func_ids.scope().lookup_id(lookup_name));
        }
        self.func_return_types
            .insert(lookup_name, call_return_type.clone());
        self.func_param_modes.insert(
            lookup_name,
            f.params.iter().map(|p| p.mode.clone()).collect(),
        );
        self.func_param_debug
            .insert(lookup_name, param_debug_from_params(&f.params));
        // Store full function type for use when the function is passed as a value.
        let param_types = f
            .params
            .iter()
            .map(|p| self.canonical_enum_type(&p.ty))
            .collect();
        self.fn_types.insert(
            lookup_name,
            Type::Fn(param_types, Box::new(call_return_type)),
        );
        Ok(())
    }

    pub(super) fn compile_function_named(
        &mut self,
        name: &str,
        f: &FunctionDecl,
        body: Option<crate::parser::ast::BodyId>,
    ) -> Result<()> {
        let func_id = self.func_ids[name];
        // `willow_user_main` is always parameterless (the runtime calls it with
        // no arguments). A declared `fn main(args: Array<String>)` parameter is
        // bound from the runtime inside the body instead of via a call argument.
        // `name` here is the lookup name (`main`), so map it to the symbol.
        let is_main = user_function_symbol(name) == USER_MAIN_SYMBOL;

        let mut sig = self.module.make_signature();
        let ptr_ty = reference_type(self.module.target_config());
        if !is_main {
            for param in &f.params {
                sig.params
                    .push(AbiParam::new(param_abi_type(param, ptr_ty)));
            }
        }
        // Stage 5 function-body cutover (willow-0g8j.3): every checked function
        // compiles from lowered IR, and since willow-t0uy.3 there is no AST
        // emitter left to fall back to — a missing or rejected LIR body is an
        // internal compile error, never a backend selection decision.
        // `main` is eligible in its `void` forms, with or without the declared
        // `args: Array<String>` (willow-0g8j.2.10): `willow_user_main` takes no
        // arguments either way, and a declared `args` is bound from the process
        // arguments below, BEFORE the body is emitted, so the walker sees an
        // ordinary local. `fn main() -> Result<void, E>` is eligible too
        // (willow-0g8j.2.14): it also lowers to a void `willow_user_main`.
        // Synchronous exits are shaped in the walker; an async poll publishes
        // the Result into its frame and the generated main driver applies the
        // same `emit_main_result_exit` shaping after joining it (willow-4ylu).
        // Any other `main` return type is rejected here.
        let supported_main = is_main
            && (f.return_type == (Type::Void).to_source() || main_result_err_type(f).is_some());
        if is_main && !supported_main {
            anyhow::bail!(
                "function `{name}` has no valid lowered body: `main` must return `void` or `Result<void, E>`"
            );
        }
        let lir_fn = self.take_lir_body(body, self.func_ids.scope().lookup_id(name))?;
        let ctx = lir_type_ctx!(
            self,
            &crate::semantic::ids::SemanticType::from(&f.return_type)
        );
        if let Some(reason) = super::lir_gen::lir_rejection_reason(&lir_fn, &ctx).or_else(|| {
            f.is_async
                .then(|| super::lir_gen::lir_async_rejection_reason(&lir_fn))
                .flatten()
        }) {
            anyhow::bail!("function `{name}` has invalid lowered IR: {reason}");
        }

        // Async functions still use the cooperative constructor/poll ABI, with
        // the same mandatory lowered body.
        if f.is_async {
            if std::env::var("WILLOW_LIR_LOG").is_ok() {
                eprintln!("[lir] compiling async `{name}` from lowered IR");
            }
            return if is_main {
                self.compile_cooperative_main(name, f, lir_fn)
            } else {
                self.compile_cooperative_leaf(name, f, lir_fn)
            };
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
            sig.returns.push(AbiParam::new(clif_type(
                reference_type(self.module.target_config()),
                &call_return_type,
            )));
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
        // Native task cancellation also uses the neutral synchronous ABI return.
        // Main has no generated caller to finish an unhandled panic.
        let panic_return_block = (!is_main).then(|| builder.create_block());

        let mut fg = FuncGen {
            builder: &mut builder,
            defer_stack: Vec::new(),
            defer_counter: 0,
            sync_defer_flags: HashMap::new(),
            panic_scopes: Vec::new(),
            unavailable_defer_ids: HashSet::new(),
            panic_defer_codegen_depth: 0,
            recover_eligible_depth: 0,
            panic_recovery_targets: HashSet::new(),
            panic_return_block,
            panic_function_root_depth: None,
            panic_depth_snapshot: None,
            emitting_sync_cancel_cleanup: false,
            sync_native_active: None,
            lir_cleanup_exit: None,
            callstack_frame_depth: 0,
            lir_call_frames: Vec::new(),
            lir_reference_scopes: Vec::new(),
            fault_site_span: None,
            collected_defer_sites: Vec::new(),
            lock_scopes: Vec::new(),
            collected_lock_sites: Vec::new(),
            collected_cleanup_order: 0,
            module: &mut self.module,
            gc_tlab_state: self.gc_tlab_state,
            gc_bitmap_descriptors: &mut self.gc_bitmap_descriptors,
            gc_layout_descriptors: &mut self.gc_layout_descriptors,
            func_ids: &self.func_ids,
            func_return_types: &self.func_return_types,
            fn_types: &self.fn_types,
            func_param_modes: &self.func_param_modes,
            func_param_debug: &self.func_param_debug,
            function_may_panic: &self.function_may_panic,
            known_modules: &self.known_modules,
            visible_modules: &self.visible_modules,
            builtin_module_aliases: &self.builtin_module_aliases,
            lambda_names: &self.lambda_names,
            string_literals: &self.string_literals,
            classes: ClassView::new(&self.type_scope, &self.layout_queries),
            static_storage: &self.static_storage,
            enum_infos: &self.enum_infos,
            class_descriptor_ids: &self.class_descriptor_ids,
            dispatch_cache: &self.dispatch_cache,
            vtable_ids: &self.vtable_ids,
            coop_frame: None,
            coop_suspend_points: None,
            coop_result_offset: None,
            async_frame: None,
            async_frame_offsets: HashMap::new(),
            lir_frame_offsets: HashMap::new(),
            lir_defer_offsets: HashMap::new(),
            main_result_err_ty,
            vars: HashMap::new(),
            current_class: None,
            // Every async function returned above through the cooperative
            // constructor/poll pair, so what is left here is synchronous.
            is_async: false,
            terminated: false,
            gc_root_count: 0,
            coop_shadow_roots: None,
            build_mode: self.build_mode,
            source_file: &self.source_file,
            address_taken: super::lir_address_taken_locals(&lir_fn),
        };
        if panic_return_block.is_some() && super::root_effect::may_push_gc_roots(&lir_fn) {
            fg.panic_function_root_depth =
                Some(fg.emit_value_runtime_call("willow_root_depth", &[]));
        }

        // Bind params
        if is_main {
            // Bind a declared `args: Array<String>` parameter from the process
            // arguments. `willow_user_main` itself takes no parameters.
            if let Some(param) = f.params.first() {
                let arr_id = fg.func_id("willow_runtime_args_array");
                let arr_ref = fg.module.declare_func_in_func(arr_id, fg.builder.func);
                let call = fg.builder.ins().call(arr_ref, &[]);
                let arr = fg.builder.inst_results(call)[0];
                fg.bind_param(
                    &param.name,
                    &crate::semantic::ids::SemanticType::from(&param.ty),
                    &param.mode,
                    arr,
                );
            }
        } else {
            for (i, param) in f.params.iter().enumerate() {
                let val = fg.builder.block_params(entry_block)[i];
                // Frame-back a GC-managed value param (its name is in the map).
                let framed = matches!(param.mode, ParamMode::Value)
                    .then(|| fg.async_frame_offsets.get(&param.span).copied())
                    .flatten();
                if let Some(offset) = framed {
                    fg.bind_param_framed(
                        &param.name,
                        &crate::semantic::ids::SemanticType::from(&param.ty),
                        val,
                        offset,
                    );
                    continue;
                }
                fg.bind_param(
                    &param.name,
                    &crate::semantic::ids::SemanticType::from(&param.ty),
                    &param.mode,
                    val,
                );
            }
        }

        // Unpack the closure environment into ordinary locals, BEFORE the body
        // runs (willow-0g8j.2.12). Each capture keeps its own declared type, so
        // a GC-managed one is rooted here by exactly the machinery that roots a
        // parameter, and the body cannot tell a capture from a parameter.
        if !lir_fn.captures.is_empty() {
            let env = fg.vars[CLOSURE_ENV_PARAM].clone();
            let env = fg.load_var(&env);
            for (i, capture) in lir_fn.captures.iter().enumerate() {
                let val = fg.builder.ins().load(
                    clif_type(reference_type(fg.module.target_config()), &capture.ty),
                    MemFlagsData::trusted(),
                    env,
                    (i as i32 + 1)
                        * willow_abi::storage_word_bytes(
                            reference_type(fg.module.target_config()).bytes(),
                        ) as i32,
                );
                fg.bind_param(&capture.name, &capture.ty, &ParamMode::Value, val);
            }
        }

        if std::env::var("WILLOW_LIR_LOG").is_ok() {
            eprintln!("[lir] compiling `{name}` from lowered IR");
        }
        fg.emit_lir_function(&lir_fn);

        // Implicit return at end of function body.
        if !fg.terminated {
            // Pop any GC roots that were pushed for parameters.
            if fg.gc_root_count > 0 {
                fg.emit_pop_roots_n(fg.gc_root_count);
            }
            if call_return_type != Type::Void && !force_void_main {
                // A value-returning fn can END with a statement whose arms all
                // return (e.g. a statement-position match, willow-zvkv); this
                // fall-through is then unreachable but must still satisfy the
                // signature.
                let zero =
                    match clif_type(reference_type(fg.module.target_config()), &call_return_type) {
                        types::F64 => fg.builder.ins().f64const(0.0),
                        ty => fg.builder.ins().iconst(ty, 0),
                    };
                fg.builder.ins().return_(&[zero]);
            } else {
                fg.builder.ins().return_(&[]);
            }
        }
        fg.emit_panic_return(&call_return_type, force_void_main);
        fg.builder.seal_all_blocks();

        builder.finalize(self.module.target_config());
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| {
                if std::env::var("WILLOW_VERIFY_DEBUG").is_ok() {
                    eprintln!("[verify] {e:?}");
                }
                e
            })?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    pub(super) fn declare_class_methods(&mut self, c: &ClassDecl) -> Result<()> {
        *self.dispatch_cache.get_mut() = Default::default();
        // Constructors lower to an ordinary `init` method (self receiver, void
        // return) so they reuse the method machinery (willow-scq2).
        let mut all_methods: Vec<MethodDecl> = c.methods.clone();
        for ctor in &c.constructors {
            all_methods.push(constructor_to_method(ctor));
        }
        for m in &all_methods {
            let mangled = self.class_method_symbol(&c.name, &m.name);
            self.func_ids.scope().declare(
                &mangled,
                FunctionId::method(TypeId::from_source_name(&c.name), &m.name),
            );
            self.claim_symbol(&mangled, format!("method `{}::{}`", c.name, m.name), m.span)?;
            let mut sig = self.module.make_signature();
            let ptr_ty = reference_type(self.module.target_config());
            sig.params
                .push(AbiParam::new(reference_type(self.module.target_config()))); // self pointer
            for p in &m.params {
                sig.params.push(AbiParam::new(param_abi_type(p, ptr_ty)));
            }
            let call_return_type = method_call_return_type(m);
            if call_return_type != Type::Void {
                sig.returns.push(AbiParam::new(clif_type(
                    reference_type(self.module.target_config()),
                    &call_return_type,
                )));
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
            let mut param_types = vec![Type::Named(c.name.clone().into())]; // self
            param_types.extend(m.params.iter().map(|p| Type::from(&p.ty)));
            self.fn_types
                .insert(mangled, Type::Fn(param_types, Box::new(call_return_type)));
        }
        Ok(())
    }

    /// Declare global storage for each `static [mut] name: T = expr` property and
    /// record its initializer for `__willow_static_init` (willow-qsqf §13.3/§11).
    /// `class_key` is the registered (possibly module-qualified) class name.
    pub(super) fn declare_static_storage_for_class(
        &mut self,
        class_key: &str,
        c: &ClassDecl,
        owner: InitUnitId,
    ) -> Result<()> {
        for field in &c.fields {
            if !field.is_static {
                continue;
            }
            let Some(init) = &field.initializer else {
                continue;
            };
            if self
                .static_storage
                .get(class_key)
                .is_some_and(|fields| fields.contains_key(&field.name))
            {
                continue;
            }
            let sym = static_property_symbol(class_key, &field.name);
            self.claim_symbol(
                &sym,
                format!("static property `{class_key}::{}`", field.name),
                field.span,
            )?;
            let data_id = self
                .module
                .declare_data(&sym, Linkage::Local, true, false)?;
            let mut data = DataDescription::new();
            // Zero-initialized: GC-managed slots start null so a collection during
            // static init sees a safe (null) slot (willow-qsqf §12.3). The slot
            // holds a pointer and is registered as a GC root, so it must be
            // 8-aligned — the collector dereferences the root slot.
            let storage_bytes =
                willow_abi::storage_word_bytes(reference_type(self.module.target_config()).bytes());
            data.define_zeroinit(storage_bytes as usize);
            data.set_align(storage_bytes as u64);
            self.module.define_data(data_id, &data)?;
            self.static_storage
                .entry(class_key.to_string())
                .or_insert(HashMap::new())
                .insert(
                    field.name.clone(),
                    StaticStorageInfo {
                        data_id,
                        ty: field.ty.clone().into(),
                    },
                );
            let initializer_name =
                crate::ir::typed_ast::static_initializer_name(class_key, &field.name);
            let initializer = FunctionDecl {
                name: initializer_name.clone(),
                public: false,
                is_async: false,
                params: Vec::new(),
                return_type: field.ty.clone(),
                body: Block {
                    id: crate::parser::ast::BodyId::fresh(),
                    stmts: Vec::new(),
                    span: init.span(),
                },
                span: init.span(),
            };
            let static_body = self.static_initializer_body(init)?;
            let symbol =
                self.class_method_symbol(class_key, &format!("$static_init.{}", field.name));
            let identity = FunctionId::method(
                TypeId::from_source_name(class_key),
                format!("$static_init.{}", field.name),
            );
            self.func_ids.scope().declare(&initializer_name, identity);
            self.func_ids.scope().declare(&symbol, identity);
            self.declare_function_symbol(&initializer_name, &symbol, &initializer, false)?;
            self.unit_static_inits
                .entry(owner)
                .or_default()
                .items
                .push(StaticInitItem {
                    class_key: class_key.to_string(),
                    field: field.name.clone(),
                    initializer,
                    body: static_body,
                    ty: field.ty.clone().into(),
                });
        }
        Ok(())
    }

    /// Compile `__willow_static_init`: evaluate every static-property initializer
    /// in declaration order, store it into global storage, and register
    /// GC-managed slots as permanent roots (willow-qsqf §11/§12). Called once at
    /// the start of `willow_user_main`.
    pub(super) fn compile_static_init(&mut self) -> Result<()> {
        let calls: Vec<FuncId> = self
            .module_init_plan
            .order()
            .iter()
            .filter(|unit| **unit != InitUnitId::Entry)
            .filter_map(|unit| {
                self.unit_static_inits
                    .get(unit)
                    .and_then(|node| node.function)
            })
            .collect();
        let items = std::mem::take(
            &mut self
                .unit_static_inits
                .entry(InitUnitId::Entry)
                .or_default()
                .items,
        );
        let func_id = self.func_ids[STATIC_INIT_SYMBOL];
        self.emit_static_init_body(func_id, &calls, &items)
    }

    /// Compile one module's static-property initializers into a private
    /// function of its own, called from `__willow_static_init` (willow-6xgo).
    ///
    /// Emitted from the module's BODY phase, so the expressions are compiled
    /// under the same aliases the module's functions are: a bare `Slot` is this
    /// module's `h::Slot`, a bare `seed()` its mangled symbol, and
    /// `Holder::base` its own storage. Compiled in the entry's phase instead,
    /// every one of those names resolved to nothing and the property silently
    /// took a zero.
    pub(super) fn compile_unit_static_init(&mut self, unit: &DeclaredModule) -> Result<()> {
        let items = std::mem::take(
            &mut self
                .unit_static_inits
                .entry(unit.init_unit)
                .or_default()
                .items,
        );
        if items.is_empty() {
            return Ok(());
        }
        let symbol = module_static_init_symbol(&unit.module_prefix);
        let sig = self.module.make_signature();
        let func_id = self
            .module
            .declare_function(&symbol, Linkage::Local, &sig)?;
        self.emit_static_init_body(func_id, &[], &items)?;
        self.unit_static_inits
            .entry(unit.init_unit)
            .or_default()
            .function = Some(func_id);
        Ok(())
    }

    /// The shared body of every static-initializer function: call `calls` in
    /// order, then store each item's value into its slot.
    fn emit_static_init_body(
        &mut self,
        func_id: FuncId,
        calls: &[FuncId],
        items: &[StaticInitItem],
    ) -> Result<()> {
        for item in items {
            self.compile_function_named(&item.initializer.name, &item.initializer, item.body)?;
        }
        let sig = self.module.make_signature();
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
            defer_stack: Vec::new(),
            defer_counter: 0,
            sync_defer_flags: HashMap::new(),
            panic_scopes: Vec::new(),
            unavailable_defer_ids: HashSet::new(),
            panic_defer_codegen_depth: 0,
            recover_eligible_depth: 0,
            panic_recovery_targets: HashSet::new(),
            panic_return_block: None,
            panic_function_root_depth: None,
            panic_depth_snapshot: None,
            emitting_sync_cancel_cleanup: false,
            sync_native_active: None,
            lir_cleanup_exit: None,
            callstack_frame_depth: 0,
            lir_call_frames: Vec::new(),
            lir_reference_scopes: Vec::new(),
            fault_site_span: None,
            collected_defer_sites: Vec::new(),
            lock_scopes: Vec::new(),
            collected_lock_sites: Vec::new(),
            collected_cleanup_order: 0,
            module: &mut self.module,
            gc_tlab_state: self.gc_tlab_state,
            gc_bitmap_descriptors: &mut self.gc_bitmap_descriptors,
            gc_layout_descriptors: &mut self.gc_layout_descriptors,
            func_ids: &self.func_ids,
            func_return_types: &self.func_return_types,
            fn_types: &self.fn_types,
            func_param_modes: &self.func_param_modes,
            func_param_debug: &self.func_param_debug,
            function_may_panic: &self.function_may_panic,
            known_modules: &self.known_modules,
            visible_modules: &self.visible_modules,
            builtin_module_aliases: &self.builtin_module_aliases,
            lambda_names: &self.lambda_names,
            string_literals: &self.string_literals,
            classes: ClassView::new(&self.type_scope, &self.layout_queries),
            static_storage: &self.static_storage,
            enum_infos: &self.enum_infos,
            class_descriptor_ids: &self.class_descriptor_ids,
            dispatch_cache: &self.dispatch_cache,
            vtable_ids: &self.vtable_ids,
            coop_frame: None,
            coop_suspend_points: None,
            coop_result_offset: None,
            async_frame: None,
            async_frame_offsets: HashMap::new(),
            lir_frame_offsets: HashMap::new(),
            lir_defer_offsets: HashMap::new(),
            main_result_err_ty: None,
            vars: HashMap::new(),
            current_class: None,
            is_async: false,
            terminated: false,
            gc_root_count: 0,
            coop_shadow_roots: None,
            build_mode: self.build_mode,
            source_file: &self.source_file,
            // Static initialisers hold no user body, so nothing takes an address.
            address_taken: HashSet::new(),
        };

        let ptr_ty = reference_type(fg.module.target_config());
        for callee in calls {
            let callee_ref = fg.module.declare_func_in_func(*callee, fg.builder.func);
            fg.builder.ins().call(callee_ref, &[]);
        }
        for item in items {
            // Initializers reference other statics by explicit class name
            // (`C::a`); `Self::` is not resolved here in the MVP.
            fg.fault_site_span = Some(item.initializer.span);
            let callee = fg.func_ids[&item.initializer.name];
            let callee = fg.module.declare_func_in_func(callee, fg.builder.func);
            let depth = fg.emit_pre_user_call_panic_depth(&item.initializer.name);
            let call = fg.builder.ins().call(callee, &[]);
            let val = fg.builder.inst_results(call)[0];
            fg.emit_post_willow_call_panic_check(depth);
            // The declaration identity, not whatever a unit's aliases say:
            // this runs once for the whole build, over keys the declaration
            // phase itself recorded.
            let info = &fg
                .static_storage
                .get_canonical(&item.class_key)
                .and_then(|fields| fields.get(&item.field))
                .expect("static property storage was declared")
                .clone();
            let gv = fg
                .module
                .declare_data_in_func(info.data_id, fg.builder.func);
            let addr = fg.builder.ins().symbol_value(ptr_ty, gv);
            fg.emit_gc_heap_store(addr, 0, val, &item.ty, GcStoreDestination::GlobalStatic);
            // GC-managed statics: root the slot permanently so the collector
            // traces the current value (also correct for `static mut`).
            if is_gc_managed(&item.ty, fg.enum_infos) {
                let push_id = fg.func_id("willow_push_root");
                let push_ref = fg.module.declare_func_in_func(push_id, fg.builder.func);
                fg.builder.ins().call(push_ref, &[addr]);
            }
        }
        fg.builder.ins().return_(&[]);
        builder.finalize(self.module.target_config());
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| {
                if std::env::var("WILLOW_VERIFY_DEBUG").is_ok() {
                    eprintln!("[verify] {e:?}");
                }
                e
            })?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// Emit a static vtable per `(class, implemented-interface)` pair. Each
    /// vtable is `slot_count` function pointers in the interface's declaration
    /// (method) order; slot K points at the concrete method the class provides
    /// (found in the class itself or an ancestor). See spec §8.2 / §9.5.
    pub(super) fn declare_interface_vtables(&mut self, program: &Program) -> Result<()> {
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
    pub(super) fn declare_vtables_for_classes(&mut self, classes: &[ClassDecl]) -> Result<()> {
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
                    crate::parser::ast::Type::Named(n)
                    | crate::parser::ast::Type::Generic(n, _) => n.clone(),
                    _ => continue,
                };
                let Some(iface) = self.classes().interface(&iface_name) else {
                    continue; // unknown interface already reported by the type checker
                };
                self.declare_one_vtable(&c.name, &iface, c.span)?;
            }
        }
        Ok(())
    }

    pub(super) fn declare_one_vtable(
        &mut self,
        class_name: &str,
        iface: &InterfaceInfo,
        span: crate::diagnostics::Span,
    ) -> Result<()> {
        let mut pending = vec![iface.name];
        let mut definitions = Vec::new();
        // Declare every reachable symbol before writing references. Explicit
        // worklists keep deep inheritance independent of the compiler stack.
        while let Some(name) = pending.pop() {
            let canonical = self
                .classes()
                .interface(&name)
                .map(|info| info.name)
                .unwrap_or(name);
            let key = (TypeId::from_source_name(class_name), canonical);
            if self.vtable_ids.contains_key(&key) {
                continue;
            }
            let info = self
                .classes()
                .interface(&canonical)
                .unwrap_or_else(|| std::sync::Arc::new(iface.clone()));
            let symbol = vtable_symbol(class_name, &info.name.to_string());
            self.claim_symbol(
                &symbol,
                format!("interface implementation `{class_name}: {}`", info.name),
                span,
            )?;
            let id = self
                .module
                .declare_data(&symbol, Linkage::Local, false, false)?;
            self.vtable_ids.insert(key, id);
            for sup in &info.extends {
                if let Some(super_info) = self.classes().interface(sup) {
                    pending.push(super_info.name);
                }
            }
            definitions.push(info);
        }
        for info in definitions {
            self.define_one_vtable(class_name, &info, span)?;
        }
        Ok(())
    }

    fn define_one_vtable(
        &mut self,
        class_name: &str,
        iface: &InterfaceInfo,
        span: crate::diagnostics::Span,
    ) -> Result<()> {
        let key = (TypeId::from_source_name(class_name), iface.name);
        let data_id = self.vtable_ids[&key];
        let slots = super::vtable_layout::slots(&self.classes(), &iface.name);
        let method_words = slots.len().max(1);
        let slot_count = method_words + iface.extends.len();
        let mut data = DataDescription::new();
        // Explicit zeroed bytes (not `define_zeroinit`, which is BSS and cannot
        // carry the function-address relocations written below).
        let pointer_bytes = reference_type(self.module.target_config()).bytes();
        data.set_align(pointer_bytes as u64);
        data.define(
            vec![
                0u8;
                willow_abi::dispatch_layout::table_slot_offset(slot_count as u32, pointer_bytes)
                    as usize
            ]
            .into_boxed_slice(),
        );
        // Filled BY NAME, which is what lets one method occupy several slots
        // (a diamond's shared grandparent) without the copies disagreeing.
        for (slot, method_name) in slots.iter().enumerate() {
            let Some(func_id) = self.resolve_class_method_func_id(class_name, method_name) else {
                continue;
            };
            // An `open`/`override` method must not be nailed to the body THIS
            // class would inherit: the box may have been built from a
            // base-typed expression whose object is really a subclass, and the
            // subclass's override has to win (willow-tygf). Its slot therefore
            // holds a thunk that re-dispatches through the receiver's own class
            // descriptor. A method with no virtual slot is neither `open` nor
            // an `override`, so no subclass can replace it and the direct
            // address is both correct and cheaper.
            //
            // Slotted methods take the thunk unconditionally rather than
            // devirtualizing a lone implementation the way `plan_virtual_call`
            // does: vtables are laid out per UNIT, and a class declared by a
            // later unit — the entry file, typically — can still contribute the
            // override that makes the count two.
            let vslot = self
                .classes()
                .slots(class_name)
                .and_then(|slots| slots.slot_of(method_name));
            let entry = match vslot {
                Some(vslot) => {
                    self.declare_vtable_thunk(class_name, method_name, vslot, func_id, span)?
                }
                None => func_id,
            };
            let func_ref = self.module.declare_func_in_data(entry, &mut data);
            data.write_function_addr(
                willow_abi::dispatch_layout::table_slot_offset(slot as u32, pointer_bytes),
                func_ref,
            );
        }
        for (index, sup) in iface.extends.iter().enumerate() {
            let canonical = self
                .classes()
                .interface(sup)
                .map(|info| info.name)
                .unwrap_or(*sup);
            if let Some(&target) = self.vtable_ids.get(&(key.0, canonical)) {
                let reference = self.module.declare_data_in_data(target, &mut data);
                data.write_data_addr(
                    willow_abi::dispatch_layout::table_slot_offset(
                        (method_words + index) as u32,
                        pointer_bytes,
                    ),
                    reference,
                    0,
                );
            }
        }
        self.module.define_data(data_id, &data)?;
        Ok(())
    }

    /// Emit (once per `(class, method)` pair) the virtual-dispatch thunk a
    /// vtable slot points at instead of a method body (willow-tygf).
    ///
    /// The thunk takes the target method's own signature, loads the receiver's
    /// class descriptor from word 0 of the object, reads virtual slot `vslot`
    /// from it, and calls that. `vslot` is the index the STATIC class assigns
    /// the method, and it is valid for every class the receiver can actually be
    /// because a subclass's slot order EXTENDS its base's — the same invariant
    /// [`FuncGen::emit_vtable_slot_load`] relies on for a class-typed call.
    ///
    /// Nothing is allocated between entry and the call, so no GC can run inside
    /// the thunk and its arguments need no roots of their own; the callee roots
    /// its parameters exactly as it does under a direct call.
    fn declare_vtable_thunk(
        &mut self,
        class_name: &str,
        method_name: &str,
        vslot: usize,
        target: FuncId,
        span: crate::diagnostics::Span,
    ) -> Result<FuncId> {
        let key = (class_name.to_string(), method_name.to_string());
        if let Some(&existing) = self.vtable_thunk_ids.get(&key) {
            return Ok(existing);
        }
        // The body it forwards to is already declared, so its signature is the
        // one authority on the thunk's own: an `override` must match the method
        // it replaces, so every class the receiver can be agrees with it.
        let sig = self
            .module
            .declarations()
            .get_function_decl(target)
            .signature
            .clone();
        if sig.params.is_empty() {
            // No receiver to read a descriptor from. Unreachable for an
            // instance method, and a vtable slot is only ever entered through
            // one, but codegen stays total rather than indexing into nothing.
            return Ok(target);
        }
        let symbol = vtable_thunk_symbol(class_name, method_name);
        // Shares the one linker namespace with every other symbol the backend
        // hands out, so it is claimed like the rest (willow-uqzx, item 8).
        self.claim_symbol(
            &symbol,
            format!("virtual dispatch thunk for `{class_name}::{method_name}`"),
            span,
        )?;
        let func_id = self
            .module
            .declare_function(&symbol, Linkage::Local, &sig)?;

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig.clone();
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());
        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let params: Vec<cranelift_codegen::ir::Value> = builder.block_params(entry).to_vec();
        let ptr_ty = reference_type(self.module.target_config());
        // Parameter 0 is the receiver for every instance method, which is what
        // a vtable slot is only ever reached through.
        let self_ptr = params[0];
        let descriptor = builder
            .ins()
            .load(ptr_ty, MemFlagsData::new(), self_ptr, 0i32);
        let offset =
            willow_abi::dispatch_layout::class_slot_offset(vslot as u32, ptr_ty.bytes()) as i32;
        let callee = builder
            .ins()
            .load(ptr_ty, MemFlagsData::new(), descriptor, offset);
        let sig_ref = builder.import_signature(sig);
        let call = builder.ins().call_indirect(sig_ref, callee, &params);
        let results = builder.inst_results(call).to_vec();
        builder.ins().return_(&results);
        builder.finalize(self.module.target_config());
        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);

        self.vtable_thunk_ids.insert(key, func_id);
        Ok(func_id)
    }

    /// Emit one descriptor per class named in `classes` (already
    /// module-qualified), plus one for every ancestor they name.
    ///
    /// Must run AFTER `declare_class_methods`, because every slot is filled by
    /// `resolve_class_method_func_id`, which reads `func_ids`.
    /// Emit a descriptor for every class in `program` (willow-fm7t).
    pub(super) fn declare_class_descriptors(&mut self, program: &Program) -> Result<()> {
        let classes: Vec<ClassDecl> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        self.declare_class_descriptors_for(&classes)
    }

    pub(super) fn declare_class_descriptors_for(&mut self, classes: &[ClassDecl]) -> Result<()> {
        for c in classes {
            self.declare_one_class_descriptor(&c.name, c.span)?;
        }
        Ok(())
    }

    /// Emit `class_name`'s descriptor: `type_id` at offset 0, then one function
    /// pointer per entry of the class's frozen virtual slot order.
    ///
    /// Every class gets one, INCLUDING a class with no virtual methods at all —
    /// otherwise word 0 of an object would sometimes be a descriptor address
    /// and sometimes a bare type_id, and no reader could tell which
    /// (willow-fm7t.1).
    pub(super) fn declare_one_class_descriptor(
        &mut self,
        class_name: &str,
        span: crate::diagnostics::Span,
    ) -> Result<()> {
        if self.class_descriptor_ids.contains_key(class_name) {
            return Ok(());
        }
        let Some(type_id) = self.classes().type_id(class_name) else {
            return Ok(()); // not a registered class; nothing to describe
        };
        let slots = self.classes().slots(class_name);
        let slots: &[String] = slots.as_ref().map_or(&[], |slots| slots.as_slice());
        let symbol = class_descriptor_symbol(class_name);
        // Shares the one linker namespace with every other symbol the backend
        // hands out, so it is claimed like the rest (willow-uqzx, item 8).
        self.claim_symbol(&symbol, format!("class `{class_name}`"), span)?;
        let data_id = self
            .module
            .declare_data(&symbol, Linkage::Local, false, false)?;
        let mut data = DataDescription::new();
        // Explicit zeroed bytes (not `define_zeroinit`, which is BSS and cannot
        // carry the function-address relocations written below) — the same
        // constraint `declare_one_vtable` works under.
        let pointer_bytes = reference_type(self.module.target_config()).bytes();
        data.set_align(willow_abi::dispatch_layout::CLASS_ID_BYTES as u64);
        let mut bytes = vec![
            0u8;
            willow_abi::dispatch_layout::class_slot_offset(slots.len() as u32, pointer_bytes)
                as usize
        ];
        // The type_id is a plain constant, not a relocation, so it is written
        // into the bytes directly — in the TARGET's byte order, which is what
        // the generated `load` will read it back in.
        let type_id_bytes = match self.module.isa().endianness() {
            cranelift_codegen::ir::Endianness::Little => type_id.to_le_bytes(),
            cranelift_codegen::ir::Endianness::Big => type_id.to_be_bytes(),
        };
        bytes[..8].copy_from_slice(&type_id_bytes);
        data.define(bytes.into_boxed_slice());
        for (slot, method_name) in slots.iter().enumerate() {
            // Resolves to an ancestor's body when this class did not redeclare
            // the method, which is exactly what an inherited slot must hold.
            if let Some(func_id) = self.resolve_class_method_func_id(class_name, method_name) {
                let func_ref = self.module.declare_func_in_data(func_id, &mut data);
                data.write_function_addr(
                    willow_abi::dispatch_layout::class_slot_offset(slot as u32, pointer_bytes),
                    func_ref,
                );
            }
        }
        self.module.define_data(data_id, &data)?;
        self.class_descriptor_ids
            .insert(class_name.to_string(), data_id);
        Ok(())
    }

    pub(super) fn compile_class_method(
        &mut self,
        c: &ClassDecl,
        m: &MethodDecl,
        body: Option<crate::parser::ast::BodyId>,
    ) -> Result<()> {
        self.compile_class_method_inner(c, m, body)
    }

    fn compile_class_method_inner(
        &mut self,
        c: &ClassDecl,
        m: &MethodDecl,
        body: Option<crate::parser::ast::BodyId>,
    ) -> Result<()> {
        let mangled = self.class_method_symbol(&c.name, &m.name);
        // LIR-walking path for a method body (willow-0g8j.2.18). `lower_program`
        // lowers every method under `Class::method` -- the key
        // `register_lir_functions` stored it under, which is not the mangled
        // symbol -- and puts the `self` receiver first in the lowered parameter
        // list, exactly where the method ABI passes it. So the receiver and
        // parameter bindings below are what the walker's body reads.
        let lir_name = FunctionId::method(TypeId::from_source_name(&c.name), &m.name);
        let lir_fn = self.take_lir_body(body, lir_name)?;
        let ctx = lir_type_ctx!(
            self,
            &crate::semantic::ids::SemanticType::from(&m.return_type)
        );
        if let Some(reason) = super::lir_gen::lir_rejection_reason(&lir_fn, &ctx).or_else(|| {
            m.is_async
                .then(|| super::lir_gen::lir_async_rejection_reason(&lir_fn))
                .flatten()
        }) {
            anyhow::bail!("method `{lir_name}` has invalid lowered IR: {reason}");
        }
        if m.is_async {
            if std::env::var("WILLOW_LIR_LOG").is_ok() {
                eprintln!("[lir] compiling async `{lir_name}` from lowered IR");
            }
            return self.compile_cooperative_method(&c.name, &mangled, m, lir_fn);
        }
        let func_id = self.func_ids[&mangled];

        let mut sig = self.module.make_signature();
        let ptr_ty = reference_type(self.module.target_config());
        sig.params
            .push(AbiParam::new(reference_type(self.module.target_config()))); // self pointer
        for p in &m.params {
            sig.params.push(AbiParam::new(param_abi_type(p, ptr_ty)));
        }
        let call_return_type = method_call_return_type(m);
        if call_return_type != Type::Void {
            sig.returns.push(AbiParam::new(clif_type(
                reference_type(self.module.target_config()),
                &call_return_type,
            )));
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
        let panic_return_block = Some(builder.create_block());

        let mut fg = FuncGen {
            builder: &mut builder,
            defer_stack: Vec::new(),
            defer_counter: 0,
            sync_defer_flags: HashMap::new(),
            panic_scopes: Vec::new(),
            unavailable_defer_ids: HashSet::new(),
            panic_defer_codegen_depth: 0,
            recover_eligible_depth: 0,
            panic_recovery_targets: HashSet::new(),
            panic_return_block,
            panic_function_root_depth: None,
            panic_depth_snapshot: None,
            emitting_sync_cancel_cleanup: false,
            sync_native_active: None,
            lir_cleanup_exit: None,
            callstack_frame_depth: 0,
            lir_call_frames: Vec::new(),
            lir_reference_scopes: Vec::new(),
            fault_site_span: None,
            collected_defer_sites: Vec::new(),
            lock_scopes: Vec::new(),
            collected_lock_sites: Vec::new(),
            collected_cleanup_order: 0,
            module: &mut self.module,
            gc_tlab_state: self.gc_tlab_state,
            gc_bitmap_descriptors: &mut self.gc_bitmap_descriptors,
            gc_layout_descriptors: &mut self.gc_layout_descriptors,
            func_ids: &self.func_ids,
            func_return_types: &self.func_return_types,
            fn_types: &self.fn_types,
            func_param_modes: &self.func_param_modes,
            func_param_debug: &self.func_param_debug,
            function_may_panic: &self.function_may_panic,
            known_modules: &self.known_modules,
            visible_modules: &self.visible_modules,
            builtin_module_aliases: &self.builtin_module_aliases,
            lambda_names: &self.lambda_names,
            string_literals: &self.string_literals,
            classes: ClassView::new(&self.type_scope, &self.layout_queries),
            static_storage: &self.static_storage,
            enum_infos: &self.enum_infos,
            class_descriptor_ids: &self.class_descriptor_ids,
            dispatch_cache: &self.dispatch_cache,
            vtable_ids: &self.vtable_ids,
            coop_frame: None,
            coop_suspend_points: None,
            coop_result_offset: None,
            async_frame: None,
            async_frame_offsets: HashMap::new(),
            lir_frame_offsets: HashMap::new(),
            lir_defer_offsets: HashMap::new(),
            main_result_err_ty: None,
            vars: HashMap::new(),
            current_class: Some(c.name.as_str()),
            // An async method returned above through `compile_cooperative_method`.
            is_async: false,
            terminated: false,
            gc_root_count: 0,
            coop_shadow_roots: None,
            build_mode: self.build_mode,
            source_file: &self.source_file,
            address_taken: super::lir_address_taken_locals(&lir_fn),
        };
        // Instance methods always bind a rooted `self`, even for scalar bodies.
        if panic_return_block.is_some()
            && (!m.is_static || super::root_effect::may_push_gc_roots(&lir_fn))
        {
            fg.panic_function_root_depth =
                Some(fg.emit_value_runtime_call("willow_root_depth", &[]));
        }

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
            fg.stack_store(self_val, self_slot);
            {
                let ptr_ty = reference_type(fg.module.target_config());
                let addr = fg.builder.ins().stack_addr(ptr_ty, self_slot, 0);
                let push_id = fg.func_id("willow_push_root");
                let push_ref = fg.module.declare_func_in_func(push_id, fg.builder.func);
                fg.builder.ins().call(push_ref, &[addr]);
                fg.gc_root_count += 1;
            }
            let receiver_ty = Type::Named(c.name.clone().into());
            let receiver_storage = VarStorage::Stack {
                slot: self_slot,
                ty: receiver_ty,
            };
            fg.vars.insert("self".to_string(), receiver_storage);
        }

        // Bind remaining method params
        for (i, p) in m.params.iter().enumerate() {
            let val = fg.builder.block_params(entry_block)[i + 1];
            fg.bind_param(
                &p.name,
                &crate::semantic::ids::SemanticType::from(&p.ty),
                &p.mode,
                val,
            );
        }

        if std::env::var("WILLOW_LIR_LOG").is_ok() {
            eprintln!("[lir] compiling `{lir_name}` from lowered IR");
        }
        fg.emit_lir_function(&lir_fn);

        if !fg.terminated {
            // Pop any GC roots (self, params) before the implicit void return.
            if fg.gc_root_count > 0 {
                fg.emit_pop_roots_n(fg.gc_root_count);
            }
            if call_return_type != Type::Void {
                // Unreachable fall-through after a body that ends with an
                // all-returning statement match (willow-zvkv): satisfy the
                // signature with a typed zero.
                let zero =
                    match clif_type(reference_type(fg.module.target_config()), &call_return_type) {
                        types::F64 => fg.builder.ins().f64const(0.0),
                        ty => fg.builder.ins().iconst(ty, 0),
                    };
                fg.builder.ins().return_(&[zero]);
            } else {
                fg.builder.ins().return_(&[]);
            }
        }
        fg.emit_panic_return(&call_return_type, false);
        fg.builder.seal_all_blocks();

        builder.finalize(self.module.target_config());
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| {
                if std::env::var("WILLOW_VERIFY_DEBUG").is_ok() {
                    eprintln!("[verify] {e:?}");
                }
                e
            })?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }
}
