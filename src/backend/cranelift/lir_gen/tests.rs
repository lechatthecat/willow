use super::*;
use crate::backend::cranelift::symbols::{backend_symbol_component, class_member_symbol};
use crate::backend::cranelift::type_index::TypeMap;
use crate::lexer::Lexer;
use crate::parser::Parser;

#[test]
fn cleanup_stack_classifier_handles_deep_defer_trees_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            use crate::ir::lowered::{LirDeferBody, LirDeferId};

            fn region(instructions: Vec<LirInst>) -> LirFunction {
                LirFunction {
                    name: "cleanup".to_string().into(),
                    is_async: false,
                    params: Vec::new(),
                    return_type: Type::Void,
                    blocks: vec![LirBlock {
                        id: BlockId(0),
                        instrs: instructions,
                        terminator: Terminator::CleanupReturn,
                        recovery: Vec::new(),
                    }],
                    locals: Vec::new(),
                    async_frame: Default::default(),
                    captures: Vec::new(),
                }
            }

            // Build the ownership tree directly: source lowering would
            // obscure this classifier's stack usage with unrelated work.
            const DEPTH: usize = 10_000;
            let mut tree = region(Vec::new());
            for _ in 0..DEPTH {
                tree = region(vec![LirInst::Defer {
                    id: LirDeferId(0),
                    body: LirDeferBody {
                        function: Box::new(tree),
                        captures: Vec::new(),
                        recovery_capable: false,
                    },
                    span: Span::dummy(),
                }]);
            }
            assert!(!cleanup_needs_task_stack(&tree));

            // A cycle only at the deepest leaf must still be discovered.
            let mut leaf = &mut tree;
            for _ in 0..DEPTH {
                let LirInst::Defer { body, .. } = &mut leaf.blocks[0].instrs[0] else {
                    unreachable!()
                };
                leaf = &mut body.function;
            }
            leaf.blocks[0].terminator = Terminator::Jump(BlockId(0));
            assert!(cleanup_needs_task_stack(&tree));
            // LirFunction's iterative Drop also runs on this small stack.
        })
        .expect("spawn classifier test thread")
        .join()
        .expect("classifier test thread");
}

/// The registration tables [`LirTypeCtx`] borrows, derived from a parsed
/// single-file program the same way `compile_program` derives them: class
/// layouts and bases from `class`/`extends`, interface and enum names from
/// their declarations, and one signature per declared symbol (free
/// functions, `Class__method`, and `Class__init` for an explicit
/// constructor). Modules are out of scope for these unit tests, so
/// `known_modules` is empty and mangling is the plain `Class__method` form.
/// One declared interface method:
/// `(name, parameter types, parameter modes, return type)`.
type IfaceMethod = (String, Vec<Type>, Vec<ParamMode>, Type);

struct TestTables {
    known: HashSet<String>,
    class_layouts: TypeMap<'static, std::sync::Arc<Vec<(String, Type)>>>,
    static_fields: HashMap<(String, String), Type>,
    class_base: TypeMap<'static, TypeId>,
    class_type_ids: TypeMap<'static, i64>,
    interfaces: HashSet<String>,
    iface_type_params: HashMap<String, Vec<String>>,
    /// Direct super-interfaces, in declaration order. The real backend
    /// keeps this on `InterfaceInfo::extends`; tests retain it separately
    /// so their widening offsets use the same layout algorithm.
    iface_supers: HashMap<String, Vec<String>>,
    /// Declared methods per interface, in declaration order — the order the
    /// backend turns into vtable slots. Inherited (`extends`) methods appear
    /// here only when the caller DESUGARED first, since composing them into
    /// `Interface::methods` is desugaring's job; [`checked_lowering`] does,
    /// the raw [`eligible`] path does not.
    iface_methods: HashMap<String, Vec<IfaceMethod>>,
    /// `(class, interface)` pairs that have a vtable, standing in for the
    /// backend's `vtable_ids`. Populated from every `implements` clause,
    /// which is what `compile_program` emits a vtable for.
    vtables: HashSet<(String, String)>,
    /// Declared enums, with the same tag rule `register_enum` uses:
    /// declaration order, starting at zero.
    enums: HashMap<String, LirEnumDef>,
    fn_types: FunctionMap<'static, Type>,
    param_modes: FunctionMap<'static, Vec<ParamMode>>,
    known_modules: ModuleSymbols<'static>,
    /// The module access names the unit under test imports.
    /// `TestTables::build` leaves it empty because the plain test programs
    /// declare no modules; a test that populates `known_modules` says here
    /// which of them the importing file can see.
    visible_modules: HashSet<String>,
    /// Builtin namespace aliases declared by the test program's own
    /// `import`s, standing in for the backend's per-file map.
    builtin_module_aliases: HashMap<String, String>,
    /// Stands in for the per-function `LirTypeCtx::return_type`. Every
    /// entry point that takes a `LirFunction` rebinds it from that
    /// function, so this default is only what a direct `supported_expr`
    /// call sees.
    ret: Type,
    /// Lifted lambda symbols by the span of the lambda expression, standing
    /// in for the backend's `lambda_names`. Registered
    /// from the lowered IR's own lambda list, so a test's symbol table and
    /// its lambda bodies cannot describe different signatures.
    lambdas: HashMap<ExprId, String>,
    /// Cooperative leaf async functions, standing in for the backend's
    /// `cooperative_leaves`. Registered from every
    /// `async fn` the test program declares, which is what
    /// `compile_program` compiles as a cooperative leaf.
    cooperative_leaves: HashSet<FunctionId>,
}

impl crate::backend::cranelift::vtable_layout::IfaceShapes for TestTables {
    fn canonical(&self, iface: &TypeId) -> TypeId {
        *iface
    }

    fn supers(&self, iface: &TypeId) -> Vec<TypeId> {
        self.iface_supers
            .get(&iface.to_string())
            .map(|supers| {
                supers
                    .iter()
                    .map(|name| TypeId::from_source_name(name))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn method_slot(&self, iface: &TypeId, method: &str) -> Option<usize> {
        self.methods(iface).iter().position(|name| name == method)
    }
    fn method_count(&self, iface: &TypeId) -> usize {
        self.methods(iface).len()
    }
    fn methods(&self, iface: &TypeId) -> Vec<String> {
        self.iface_methods
            .get(&iface.to_string())
            .map(|methods| methods.iter().map(|(name, ..)| name.clone()).collect())
            .unwrap_or_default()
    }
}

impl ClassTables for TestTables {
    fn class_fields(&self, class: &TypeId) -> Option<std::sync::Arc<Vec<(String, Type)>>> {
        self.class_layouts.get(class).cloned()
    }

    fn class_type_id(&self, class: &TypeId) -> Option<i64> {
        self.class_type_ids.get(class).copied()
    }

    fn class_base(&self, class: &TypeId) -> Option<TypeId> {
        self.class_base.get(class).copied()
    }
}

impl TestTables {
    fn build(
        program: &crate::parser::ast::Program,
        extra_fns: &[&str],
        lambdas: &[crate::ir::lowered::LirLambda],
    ) -> Self {
        use crate::parser::ast::Item;
        let mut t = TestTables {
            known: extra_fns.iter().map(|s| s.to_string()).collect(),
            cooperative_leaves: HashSet::new(),
            class_layouts: TypeMap::new(),
            static_fields: HashMap::new(),
            class_base: TypeMap::new(),
            class_type_ids: TypeMap::new(),
            interfaces: HashSet::new(),
            iface_type_params: HashMap::new(),
            iface_supers: HashMap::new(),
            iface_methods: HashMap::new(),
            vtables: HashSet::new(),
            enums: HashMap::new(),
            fn_types: FunctionMap::default(),
            param_modes: FunctionMap::default(),
            known_modules: ModuleSymbols::default(),
            visible_modules: HashSet::new(),
            builtin_module_aliases:
                crate::backend::cranelift::std_collection::builtin_module_aliases(program),
            ret: Type::Void,
            lambdas: HashMap::new(),
        };
        // Every lifted lambda is a declared, linkable symbol with the
        // signature its lowered body carries — what `declare_lambda` does
        // in the backend.
        for l in lambdas {
            let name = l.function.name.to_string();
            t.known.insert(name.clone());
            t.fn_types.insert(
                &name,
                Type::Fn(
                    l.function.params.iter().map(|p| p.ty.clone()).collect(),
                    Box::new(l.function.return_type.clone()),
                ),
            );
            t.param_modes.insert(
                &name,
                l.function.params.iter().map(|_| ParamMode::Value).collect(),
            );
            t.lambdas.insert(l.id, name);
        }
        let sig = |params: &[crate::parser::ast::Param],
                   ret: &crate::parser::ast::Type,
                   with_self: bool| {
            let mut ps: Vec<Type> = Vec::new();
            if with_self {
                ps.push(Type::I64);
            }
            ps.extend(params.iter().map(|p| Type::from(&p.ty)));
            Type::Fn(ps, Box::new(ret.into()))
        };
        let modes = |params: &[crate::parser::ast::Param], with_self: bool| {
            let mut ms: Vec<ParamMode> = Vec::new();
            if with_self {
                ms.push(ParamMode::Value);
            }
            ms.extend(params.iter().map(|p| p.mode.clone()));
            ms
        };
        // The prelude's enums (`Option`, `Result`, `IoError`, …) are
        // registered the way `register_prelude` registers them with the
        // checker, before the program's own items — so a test source that
        // declares an enum of the same name still shadows them, and the
        // walker sees the same enum table the real dispatch site builds
        // from `enum_infos`.
        let prelude_tokens = Lexer::new(crate::prelude::PRELUDE_SOURCE)
            .tokenize()
            .expect("prelude lexes");
        let (prelude, prelude_errs) = Parser::new(prelude_tokens).parse();
        assert!(prelude_errs.is_empty(), "{prelude_errs:?}");
        for item in prelude.items.iter().chain(program.items.iter()) {
            match item {
                // A free function's SIGNATURE is always recorded, but its
                // name is a known symbol only when the test lists it: that
                // is how a test models a callee the backend cannot link.
                Item::Function(f) => {
                    t.fn_types
                        .insert(&f.name, sig(&f.params, &f.return_type, false));
                    t.param_modes.insert(&f.name, modes(&f.params, false));
                    if f.is_async {
                        t.cooperative_leaves
                            .insert(FunctionId::free_from_source_name(&f.name));
                    }
                }
                Item::Interface(i) => {
                    t.interfaces.insert(i.name.clone());
                    t.iface_type_params
                        .insert(i.name.clone(), i.type_params.clone());
                    t.iface_supers.insert(i.name.clone(), i.extends.clone());
                    t.iface_methods.insert(
                        i.name.clone(),
                        i.methods
                            .iter()
                            .map(|m| {
                                (
                                    m.name.clone(),
                                    m.params.iter().map(|p| Type::from(&p.ty)).collect(),
                                    m.params.iter().map(|p| p.mode.clone()).collect(),
                                    m.return_type.clone().into(),
                                )
                            })
                            .collect(),
                    );
                }
                Item::Enum(e) => {
                    t.enums.insert(
                        e.name.clone(),
                        LirEnumDef {
                            identity: e.name.clone().into(),
                            type_params: e.type_params.iter().map(TypeId::local).collect(),
                            variants: e
                                .variants
                                .iter()
                                .map(|v| LirEnumVariant {
                                    name: v.name.clone(),
                                    payloads: v.payload.iter().map(Into::into).collect(),
                                })
                                .collect(),
                        },
                    );
                }
                Item::Class(c) => {
                    for field in c.fields.iter().filter(|f| f.is_static) {
                        t.static_fields.insert(
                            (c.name.clone(), field.name.clone()),
                            field.ty.clone().into(),
                        );
                    }
                    t.class_layouts.insert(
                        c.name.clone(),
                        c.fields
                            .iter()
                            .filter(|f| !f.is_static)
                            .map(|f| (f.name.clone(), f.ty.clone().into()))
                            .collect::<Vec<_>>()
                            .into(),
                    );
                    if let Some(base) = &c.base_class {
                        t.class_base
                            .insert(c.name.clone(), base.name().to_string().into());
                    }
                    for iface in &c.implements {
                        if let crate::parser::ast::Type::Named(n)
                        | crate::parser::ast::Type::Generic(n, _) = iface
                        {
                            t.vtables.insert((c.name.clone(), n.clone()));
                        }
                    }
                    // Same rule as `register_class`: one id per class, in
                    // declaration order.
                    let next_id = i64::from(
                        willow_abi::runtime_type_ids::generated_type_id(t.class_type_ids.len())
                            .expect("test class IDs fit the generated range"),
                    );
                    t.class_type_ids.entry(c.name.clone()).or_insert(next_id);
                    // Every class method — instance, static, or a
                    // constructor lowered to `init` — carries a hidden
                    // `self` parameter in its signature; a STATIC one is
                    // simply called with a null receiver. `func_param_modes`
                    // records only the declared parameters, matching
                    // `declare_class_methods`.
                    for ctor in &c.constructors {
                        let mangled =
                            class_member_symbol(&backend_symbol_component(&c.name), "init");
                        t.known.insert(mangled.clone());
                        t.fn_types.insert(
                            &mangled,
                            sig(&ctor.params, &crate::parser::ast::Type::Void, true),
                        );
                        t.param_modes.insert(&mangled, modes(&ctor.params, false));
                    }
                    for m in &c.methods {
                        let mangled =
                            class_member_symbol(&backend_symbol_component(&c.name), &m.name);
                        t.known.insert(mangled.clone());
                        t.fn_types
                            .insert(&mangled, sig(&m.params, &m.return_type, true));
                        t.param_modes.insert(&mangled, modes(&m.params, false));
                    }
                }
            }
        }
        // Prepend each class's inherited fields, root-down, exactly as
        // `Codegen::finalize_class_layouts` does. A separate pass because a
        // subclass may be DECLARED before its base; without
        // it these tables would model an inheriting class as owning only
        // its own fields, which is not what the walker is handed.
        let own = t.class_layouts.clone();
        for (class_name, _) in own.iter() {
            let mut chain = vec![*class_name];
            let mut seen = HashSet::from([*class_name]);
            while let Some(base) = t.class_base.get(chain.last().expect("non-empty")) {
                if !seen.insert(*base) {
                    break;
                }
                chain.push(*base);
            }
            let mut fields: Vec<(String, Type)> = Vec::new();
            for ancestor in chain.iter().rev() {
                let Some(ancestor_own) = own.get(ancestor) else {
                    continue;
                };
                for (name, ty) in ancestor_own.iter() {
                    if !fields.iter().any(|(n, _)| n == name) {
                        fields.push((name.clone(), ty.clone()));
                    }
                }
            }
            t.class_layouts.insert(*class_name, fields.into());
        }
        t
    }

    /// The closures in [`LirTypeCtx`] are borrowed, so the context cannot
    /// outlive this call — hand it to the caller instead of returning it.
    fn with_ctx<R>(&self, body: impl FnOnce(&LirTypeCtx<'_>) -> R) -> R {
        body(&LirTypeCtx {
            known_fn: &|n| self.known.contains(n),
            known_fn_id: &|id| self.known.contains(&id.to_string()),
            classes: self,
            is_interface: &|n| self.interfaces.contains(&n.to_string()),
            iface_identity: &|n| self.interfaces.contains(&n.to_string()).then_some(*n),
            can_box: &|class, iface| {
                self.vtables
                    .contains(&(class.to_string(), iface.to_string()))
            },
            enum_def: &|n| self.enums.get(&n.to_string()).cloned(),
            lambda_symbol: &|id| self.lambdas.get(&id).map(FunctionId::free),
            cooperative_leaves: &self.cooperative_leaves,
            iface_method: &|iface_ty, method| {
                let (iface, args): (&TypeId, &[Type]) = match iface_ty {
                    Type::Named(name) => (name, &[]),
                    Type::Generic(name, args) => (name, args),
                    _ => return None,
                };
                let methods = self.iface_methods.get(&iface.to_string())?;
                let type_params = self.iface_type_params.get(&iface.to_string())?;
                if type_params.len() != args.len() {
                    return None;
                }
                let mut substitutions: HashMap<TypeId, Type> = type_params
                    .iter()
                    .map(TypeId::local)
                    .zip(args.iter().cloned())
                    .collect();
                substitutions.insert(TypeId::local("Self"), iface_ty.clone());
                let slot = methods.iter().position(|(n, _, _, _)| n == method)?;
                let (_, params, modes, ret) = &methods[slot];
                Some(IfaceMethodSig {
                    params: params
                        .iter()
                        .map(|ty| crate::semantic::symbols::substitute_type(ty, &substitutions))
                        .collect(),
                    modes: modes.clone(),
                    ret: crate::semantic::symbols::substitute_type(ret, &substitutions),
                })
            },
            static_field: &|class, field| {
                let mut search = Some(class.to_string());
                let mut seen = HashSet::new();
                while let Some(name) = search {
                    if !seen.insert(name.clone()) {
                        break;
                    }
                    if let Some(ty) = self.static_fields.get(&(name.clone(), field.to_string())) {
                        return Some(ty.clone());
                    }
                    search = self.class_base.get(&name).map(ToString::to_string);
                }
                None
            },
            iface_widen_path: &|target, source| {
                crate::backend::cranelift::vtable_layout::super_path(self, source, target)
            },
            fn_types: &self.fn_types,
            func_param_modes: &self.param_modes,
            known_modules: &self.known_modules,
            visible_modules: &self.visible_modules,
            builtin_module_aliases: &self.builtin_module_aliases,
            return_type: &self.ret,
            self_class: None,
        })
    }
}

fn eligible(src: &str, name: &str, fns: &[&str]) -> bool {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "{errs:?}");
    let (hir, diags) = crate::ir::lower::lower_program(&program);
    assert!(diags.is_empty(), "{diags:?}");
    let p = crate::ir::lowered::lower_program(&hir);
    let tables = TestTables::build(&program, fns, &p.lambdas);
    let f = p
        .functions
        .iter()
        .find(|f| f.name.to_string() == name)
        .unwrap();
    tables.with_ctx(|ctx| lir_supported_function(f, ctx))
}

/// Like [`eligible`], but for forms the HIR may refuse to lower at all: a
/// function with no lowered IR is by definition not claimed by the LIR path.
fn eligible_lenient(src: &str, name: &str, fns: &[&str]) -> bool {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "{errs:?}");
    let (hir, diags) = crate::ir::lower::lower_program(&program);
    if !diags.is_empty() {
        return false;
    }
    let p = crate::ir::lowered::lower_program(&hir);
    let tables = TestTables::build(&program, fns, &p.lambdas);
    match p.functions.iter().find(|f| f.name.to_string() == name) {
        Some(f) => tables.with_ctx(|ctx| lir_supported_function(f, ctx)),
        None => false,
    }
}

/// Use the frontend's bundled declarations, generic specialization and
/// checked signatures before lowering. The raw eligibility helpers remain
/// available for malformed-HIR tests that intentionally bypass semantics.
/// Fixtures without an entry point receive a temporary empty main, which
/// is removed from the returned LIR so function-count assertions stay exact.
fn checked_lowering(src: &str, fns: &[&str]) -> (crate::ir::lowered::LirProgram, TestTables) {
    let (parsed, errors) = Parser::new(Lexer::new(src).tokenize().expect("lex")).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let added_entry = !parsed.items.iter().any(|item| matches!(item, crate::parser::ast::Item::Function(function) if function.name == "main"));
    let source = if added_entry {
        format!("{src}\nfn main() {{}}")
    } else {
        src.to_owned()
    };
    let src = source.as_str();
    let map = crate::diagnostics::SourceMap::new("<backend-test>.wi", src);
    let frontend = crate::run_frontend(
        src,
        std::path::Path::new("."),
        &map,
        &crate::CompilerOptions::debug(),
    )
    .expect("checked frontend");
    let artifacts = frontend
        .module_graph
        .artifacts
        .as_ref()
        .expect("source artifacts");
    let program = artifacts
        .hydrate(&frontend.program, crate::diagnostics::FileId::ENTRY)
        .expect("entry body");
    let checked = frontend
        .db
        .checked_unit(crate::module::UnitId::ENTRY, artifacts)
        .expect("checked entry");
    let facts = checked.tables();
    let (hir, diagnostics) = crate::ir::lower::lower_program_with(&program, &facts);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let mut lir = crate::ir::lowered::lower_program(&hir);
    if added_entry {
        lir.functions
            .retain(|function| function.name.to_string() != "main");
    }
    let mut declarations = program.clone();
    normalize_test_signatures(&mut declarations, &facts);
    for module in &frontend.module_graph.files {
        let checked = frontend
            .db
            .checked_unit(module.id, artifacts)
            .expect("checked source module");
        let mut source = module.program.clone();
        normalize_test_signatures(&mut source, &checked.tables());
        for item in &mut source.items {
            if let crate::parser::ast::Item::Class(class) = item {
                class.name = format!("{}::{}", module.identity_path(), class.name);
            }
        }
        declarations.items.extend(source.items);
    }
    let tables = TestTables::build(&declarations, fns, &lir.lambdas);
    (lir, tables)
}

fn normalize_test_signatures(
    program: &mut crate::parser::ast::Program,
    facts: &crate::ir::lower::SemanticFacts<'_>,
) {
    use crate::parser::ast::Item;
    let normalize_params = |params: &mut Vec<crate::parser::ast::Param>| {
        for param in params {
            param.ty = facts.normalize(&param.ty);
        }
    };
    for item in &mut program.items {
        match item {
            Item::Function(function) => {
                normalize_params(&mut function.params);
                function.return_type = facts.normalize(&function.return_type);
            }
            Item::Class(class) => {
                for field in &mut class.fields {
                    field.ty = facts.normalize(&field.ty);
                }
                for method in &mut class.methods {
                    normalize_params(&mut method.params);
                    method.return_type = facts.normalize(&method.return_type);
                }
                for constructor in &mut class.constructors {
                    normalize_params(&mut constructor.params);
                }
            }
            Item::Interface(interface) => {
                for method in &mut interface.methods {
                    normalize_params(&mut method.params);
                    method.return_type = facts.normalize(&method.return_type);
                }
            }
            Item::Enum(declaration) => {
                for variant in &mut declaration.variants {
                    for ty in &mut variant.payload {
                        *ty = facts.normalize(ty);
                    }
                }
            }
        }
    }
}

#[test]
fn divergent_preparations_scale_with_nesting() {
    use crate::ir::lowered::LirRvalue as V;
    for depth in [8, 32, 128, 1024] {
        let expression = format!(
            "{}panic(\"stop\"){}",
            "c.take(&x, ".repeat(depth),
            ")".repeat(depth),
        );
        let source = format!(
            r#"class C {{ pub fn take(self, x: &i64, y: i64) -> i64 {{ return y; }} }}
                fn main() {{ let c = new C(); let x = 42;
                    if true {{ defer match recover() {{ Some(_) => {{}}, None => {{}} }};
                        {expression};
                    }}
                    c.take(&x, 1);
                }}"#,
        );
        let (program, _) = checked_lowering(&source, &["main"]);
        let function = program
            .functions
            .iter()
            .find(|f| f.name.is_free_named("main"))
            .unwrap();
        assert!(lir_call_frame_entries(function).is_some());
        assert!(lir_reference_scope_entries(function).is_some());
        let mut counts = [0; 4];
        for instruction in function.blocks.iter().flat_map(|b| &b.instrs) {
            if let LirInst::Compute { value, .. } = instruction {
                match value {
                    V::PrepareMethod { .. } => counts[0] += 1,
                    V::BeginReferenceCall => counts[1] += 1,
                    V::Panic { .. } => counts[2] += 1,
                    V::MethodCall { .. } => counts[3] += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(counts, [depth + 1, depth + 1, 1, 1]);
        println!("divergent preparations: depth={depth}, counts={counts:?}");
    }
}

#[test]
fn flat_graph_rejects_normal_and_recovery_edges_to_entry() {
    let (program, tables) = checked_lowering("fn main() {}", &["main"]);
    let original = &program.functions[0];
    for recovery in [false, true] {
        let mut function = original.clone();
        if recovery {
            function.blocks[0].recovery = vec![BlockId(0)];
        } else {
            function.blocks[0].terminator = Terminator::Jump(BlockId(0));
        }
        let reason = tables.with_ctx(|ctx| lir_rejection_reason(&function, ctx));
        assert_eq!(
            reason.as_deref(),
            Some("the LIR entry block must not have incoming edges")
        );
    }
}

#[test]
fn flat_array_references_reject_forged_capture_metadata() {
    use crate::ir::lowered::{LirOperand as O, LirPlace, LirRvalue as V};
    let (program, tables) = checked_lowering(
        "import std::collections::Array; fn read(x: & i64) -> i64 { return x; } fn f(a: Array<i64>, i: i64) -> i64 { return read(&a[i]); }",
        &["read", "f"],
    );
    let original = program
        .functions
        .iter()
        .find(|f| f.name.to_string() == "f")
        .unwrap();
    let original_reason = tables.with_ctx(|ctx| lir_rejection_reason(original, ctx));
    assert!(original_reason.is_none(), "{original_reason:?}");
    for forge_element in [false, true] {
        let mut function = original.clone();
        let mut changed = false;
        for block in &mut function.blocks {
            for inst in &mut block.instrs {
                if let LirInst::Compute {
                    value: V::DirectCall { args, .. },
                    ..
                } = inst
                {
                    for argument in args {
                        if let O::Reference {
                            place: LirPlace::ArrayElement { index, element, .. },
                            ..
                        } = argument
                        {
                            if forge_element {
                                *element = Type::String;
                            } else {
                                *index = original
                                    .locals
                                    .iter()
                                    .find(|local| local.parameter && local.ty == Type::I64)
                                    .unwrap()
                                    .id;
                            }
                            changed = true;
                        }
                    }
                }
            }
        }
        assert!(changed);
        let reason = tables
            .with_ctx(|ctx| lir_rejection_reason(&function, ctx))
            .expect("forged reference rejected");
        assert!(
            reason.contains("captured buffer or checked index"),
            "{reason}"
        );
    }
}

#[test]
fn flat_rvalues_reject_unknown_layouts_and_invalid_coercions() {
    use crate::ir::lowered::{LirOperand as O, LirRvalue as V};
    let (_, tables) = checked_lowering(
        "class Point { pub x: i64; pub fn get(self) -> i64 { return self.x; } } fn main() {}",
        &["main"],
    );
    tables.with_ctx(|ctx| {
        let point = Type::Named(TypeId::local("Point"));
        assert!(flat_method_signature(&point, "get", ctx).is_some());
        assert!(flat_method_signature(&point, "absent", ctx).is_none());
        assert!(!flat_rvalue_supported(
            &V::MethodCall {
                receiver: O::Int(0),
                receiver_ty: point.clone(),
                method: "get".into(),
                is_async: false,
                args: vec![],
                arg_types: vec![],
                result: Type::String,
                super_class: None,
            },
            &[],
            ctx
        ));
        assert!(flat_rvalue_supported(
            &V::ObjectAlloc {
                class: TypeId::local("Point")
            },
            &[],
            ctx
        ));
        assert!(!flat_rvalue_supported(
            &V::ObjectAlloc {
                class: TypeId::local("Missing")
            },
            &[],
            ctx
        ));
        assert!(!flat_rvalue_supported(
            &V::Coerce {
                value: O::Int(1),
                source: Type::I64,
                target: Type::String
            },
            &[],
            ctx
        ));
        assert!(!flat_rvalue_supported(
            &V::FieldLoad {
                object: O::Int(0),
                object_ty: point.clone(),
                field: "missing".into(),
                result: Type::I64
            },
            &[],
            ctx
        ));
        assert!(!flat_rvalue_supported(
            &V::FieldLoad {
                object: O::Int(0),
                object_ty: point,
                field: "x".into(),
                result: Type::String
            },
            &[],
            ctx
        ));
        assert!(!flat_rvalue_supported(
            &V::ArrayAlloc {
                length: 1,
                element: Type::Void
            },
            &[],
            ctx
        ));
        assert!(!flat_rvalue_supported(
            &V::StaticField {
                class: TypeId::local("Point"),
                field: "missing".into(),
                result: Type::I64
            },
            &[],
            ctx
        ));
    });
}

#[test]
fn flat_rvalues_reject_forged_runtime_and_enum_metadata() {
    use crate::ir::lowered::{LirOperand as O, LirRvalue as V};
    let (_, tables) = checked_lowering("fn main() {}", &["main"]);
    tables.with_ctx(|ctx| {
        assert!(flat_rvalue_supported(
            &V::BuiltinCall {
                callee: FunctionId::free("gc_collect"),
                args: vec![],
                params: vec![],
                result: Type::Void
            },
            &[],
            ctx
        ));
        assert!(!flat_rvalue_supported(
            &V::BuiltinCall {
                callee: FunctionId::free("gc_collect"),
                args: vec![],
                params: vec![],
                result: Type::I64
            },
            &[],
            ctx
        ));
        assert!(!flat_rvalue_supported(
            &V::BuiltinCall {
                callee: FunctionId::free("unknown"),
                args: vec![],
                params: vec![],
                result: Type::Void
            },
            &[],
            ctx
        ));
        assert!(!flat_rvalue_supported(
            &V::EnumAlloc {
                class: TypeId::local("Missing"),
                variant: "Some".into(),
                enum_ty: Type::Named(TypeId::local("Missing"))
            },
            &[],
            ctx
        ));
        assert!(!flat_rvalue_supported(
            &V::EnumMethod {
                receiver: O::Int(0),
                receiver_ty: Type::I64,
                method: "unwrap".into(),
                args: vec![],
                arg_types: vec![],
                result: Type::I64
            },
            &[],
            ctx
        ));
    });
}

/// [`eligible`] for constructs that need the checker's types to lower.
fn eligible_checked(src: &str, name: &str, fns: &[&str]) -> bool {
    let (p, tables) = checked_lowering(src, fns);
    match p.functions.iter().find(|f| f.name.to_string() == name) {
        Some(f) => tables.with_ctx(|ctx| lir_supported_function(f, ctx)),
        None => false,
    }
}

/// The validation diagnostic for `name`, through the same checked pipeline as
/// [`eligible_checked`]. Panics if the function did not survive lowering,
/// which would mean the test is exercising a HIR gap and not this code.
fn reason_of(src: &str, name: &str, fns: &[&str]) -> Option<String> {
    let (p, tables) = checked_lowering(src, fns);
    let f = p
        .functions
        .iter()
        .find(|f| f.name.to_string() == name)
        .unwrap_or_else(|| panic!("`{name}` has no lowered IR"));
    tables.with_ctx(|ctx| lir_rejection_reason(f, ctx))
}

/// The reason as a string, asserting there IS one.
fn rejected(src: &str, name: &str, fns: &[&str]) -> String {
    reason_of(src, name, fns).unwrap_or_else(|| panic!("`{name}` was accepted"))
}

/// The lowered function plus its (mutable) registration tables, for tests
/// that need to perturb a table the way a registration or desugaring bug
/// would and re-ask the predicate. Source alone cannot produce such a state
/// — the type checker rejects it long before lowering.
#[test]
fn bounded_scalar_return_defers_poll_but_preserves_recursive_path() {
    let (f, _) = lir_fn_and_tables(
        "fn fib(n: i64) -> i64 { if n < 2 { return n; } return fib(n-1) + fib(n-2); }",
        "fib",
        &["fib"],
    );
    let mut poll = lir_sync_poll_blocks(&f);
    assert!(lir_defer_entry_poll(&f, &mut poll, false));
    assert!(!poll[0]);
    let mut pending = vec![0];
    let mut visited = HashSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) || poll[id] {
            continue;
        }
        let block = &f.blocks[id];
        assert!(
            !block.instrs.iter().any(|i| matches!(
                i,
                LirInst::Compute {
                    value: crate::ir::lowered::LirRvalue::DirectCall { .. },
                    ..
                }
            )),
            "a recursive call must be preceded by a poll"
        );
        pending.extend(lir_block_successors(block));
    }
}

#[test]
fn entry_poll_stays_before_faults_cleanup_and_cycles() {
    for source in [
        "fn f(n: i64) -> i64 { if 10 / n < 2 { return n; } return f(n-1) + 1; }",
        "fn f(n: i64) -> i64 { defer { println(n); } if n < 2 { return n; } return f(n-1) + 1; }",
        "fn f(n: i64) -> i64 { if n < 2 { return n; } while true {} return n; }",
    ] {
        let (f, _) = lir_fn_and_tables(source, "f", &["f"]);
        let mut poll = lir_sync_poll_blocks(&f);
        let original = poll.clone();
        assert!(!lir_defer_entry_poll(&f, &mut poll, false), "{source}");
        assert_eq!(poll, original);
    }
}

#[test]
fn entry_poll_stays_before_debug_overflow_checks() {
    let (f, _) = lir_fn_and_tables(
        "fn f(n: i64) -> i64 { if n * 3 < 2 { return n; } return f(n-1) + 1; }",
        "f",
        &["f"],
    );
    let mut poll = lir_sync_poll_blocks(&f);
    let original = poll.clone();
    assert!(!lir_defer_entry_poll(&f, &mut poll, true));
    assert_eq!(poll, original);
    assert!(lir_defer_entry_poll(&f, &mut poll, false));
    assert!(!poll[0]);
}

#[test]
fn local_maps_confinement_visits_each_instruction_once() {
    for key in ["i64", "String"] {
        for count in [16, 128, 1024] {
            let source = format!(
                "import std::collections::Map; fn probe() {{ let m: Map<{key},i64> = Map::new(); {} }}",
                "m.len();".repeat(count)
            );
            let (program, _) = checked_lowering(&source, &[]);
            let f = program
                .functions
                .iter()
                .find(|f| f.name.to_string() == "probe")
                .unwrap();
            super::super::local_maps::take_visits();
            assert!(super::super::local_maps::confined_scalar_maps(f));
            let visits = super::super::local_maps::take_visits();
            assert_eq!(
                visits,
                f.blocks.iter().map(|b| b.instrs.len() + 1).sum::<usize>()
            );
            eprintln!("local-map key={key} call_sites={count} analysis_visits={visits}");
        }
    }
}

#[test]
fn local_maps_confinement_twenty_perspectives() {
    let declaration = "let mut m: Map<i64, i64> = Map::new();";
    for body in [
        "m.insert(1, 2);",
        "m.get(1);",
        "m.contains(1);",
        "m.len();",
        "m.toString();",
        "m.freeze();",
        "let a = m; a.insert(1, 2);",
        "m = Map::new(); m.insert(1, 2);",
        "let a = m; m = a;",
        "let mut i = 0; while i < 10 { m.insert(i, i); i = i + 1; }",
        "println(match m.get(1) { Some(x) => x, None => 0 });",
        "if true { m.insert(1, 2); } else { m.insert(3, 4); }",
    ] {
        let source = format!("import std::collections::Map; fn probe() {{ {declaration} {body} }}");
        let (program, _) = checked_lowering(&source, &[]);
        let f = program
            .functions
            .iter()
            .find(|f| f.name.to_string() == "probe")
            .unwrap();
        assert!(
            super::super::local_maps::confined_scalar_maps(f),
            "{body}: {f:#?}"
        );
    }
    for body in [
        "Mutex::new(m);",
        "BlockingCell::new(m);",
        "let xs = [m];",
        "Option::Some(m);",
        "let f = || m.len();",
        "defer { m.len(); }",
    ] {
        let source = format!("import std::collections::Map; fn probe() {{ {declaration} {body} }}");
        let (program, _) = checked_lowering(&source, &[]);
        let f = program
            .functions
            .iter()
            .find(|f| f.name.to_string() == "probe")
            .unwrap();
        assert!(!super::super::local_maps::confined_scalar_maps(f), "{body}");
    }
    for source in [
        "fn probe(m: Map<i64,i64>) { m.insert(1,2); }",
        "fn probe() -> Map<i64,i64> { return Map::new(); }",
        "fn consume(m: Map<i64,i64>) {} fn probe() { let m: Map<i64,i64> = Map::new(); consume(m); }",
        "fn probe() { let m: Map<i64,String> = Map::new(); m.insert(1,\"x\"); }",
        "async fn probe() { let m: Map<i64,i64> = Map::new(); m.insert(1,2); }",
    ] {
        let source = format!("import std::collections::Map; {source}");
        let (program, _) = checked_lowering(&source, &[]);
        let f = program
            .functions
            .iter()
            .find(|f| f.name.to_string() == "probe")
            .unwrap();
        assert!(
            !super::super::local_maps::confined_scalar_maps(f),
            "{source}"
        );
    }
}

#[test]
fn local_string_maps_confinement_twenty_perspectives() {
    let declaration = "let mut m: Map<String, i64> = Map::new();";
    for body in [
        r#"m.insert("x", 2);"#,
        r#"m.get("x");"#,
        r#"m.contains("x");"#,
        "m.len();",
        "m.toString();",
        "m.freeze();",
        r#"let a = m; a.insert("x", 2);"#,
        r#"m = Map::new(); m.insert("x", 2);"#,
        "let a = m; m = a;",
        r#"let mut i = 0; while i < 10 { m.insert(i.toString(), i); i = i + 1; }"#,
        r#"println(match m.get("x") { Some(x) => x, None => 0 });"#,
        r#"if true { m.insert("x", 2); } else { m.insert("y", 4); }"#,
    ] {
        let source = format!("import std::collections::Map; fn probe() {{ {declaration} {body} }}");
        let (program, _) = checked_lowering(&source, &[]);
        let f = program
            .functions
            .iter()
            .find(|f| f.name.to_string() == "probe")
            .unwrap();
        assert!(super::super::local_maps::confined_scalar_maps(f), "{body}");
    }
    for body in [
        "Mutex::new(m);",
        "BlockingCell::new(m);",
        "let xs = [m];",
        "Option::Some(m);",
        "let f = || m.len();",
        "defer { m.len(); }",
    ] {
        let source = format!("import std::collections::Map; fn probe() {{ {declaration} {body} }}");
        let (program, _) = checked_lowering(&source, &[]);
        let f = program
            .functions
            .iter()
            .find(|f| f.name.to_string() == "probe")
            .unwrap();
        assert!(!super::super::local_maps::confined_scalar_maps(f), "{body}");
    }
    for source in [
        r#"fn probe(m: Map<String,i64>) { m.insert("x",2); }"#,
        "fn probe() -> Map<String,i64> { return Map::new(); }",
        "fn consume(m: Map<String,i64>) {} fn probe() { let m: Map<String,i64> = Map::new(); consume(m); }",
        r#"fn probe() { let m: Map<String,String> = Map::new(); m.insert("x","y"); }"#,
        r#"async fn probe() { let m: Map<String,i64> = Map::new(); m.insert("x",2); }"#,
    ] {
        let source = format!("import std::collections::Map; {source}");
        let (program, _) = checked_lowering(&source, &[]);
        let f = program
            .functions
            .iter()
            .find(|f| f.name.to_string() == "probe")
            .unwrap();
        assert!(
            !super::super::local_maps::confined_scalar_maps(f),
            "{source}"
        );
    }
}

#[test]
fn local_string_maps_word_count_confinement() {
    let source = r#"import std::collections::Map;
import std::collections::Array;
fn main() {
 let words: Array<String> = [];
 let mut i = 0;
 while i < 4096 { words.push("token_" + i.toString() + "_" + (i * 31).toString()); i = i + 1; }
 let counts: Map<String, i64> = Map::new();
 i = 0;
 while i < 6000000 {
  let word = words[i % 4096];
  let count = match counts.get(word) { Some(v) => v, None => 0 };
  counts.insert(word, count + 1);
  i = i + 1;
 }
 let mut sum = 0;
 i = 0;
 while i < 4096 { sum = sum + counts.get(words[i]).unwrap(); i = i + 1; }
 println(sum);
}"#;
    let (program, _) = checked_lowering(source, &[]);
    let f = program
        .functions
        .iter()
        .find(|f| f.name.to_string() == "main")
        .unwrap();
    assert!(super::super::local_maps::confined_scalar_maps(f));
}

fn lir_fn_and_tables(src: &str, name: &str, fns: &[&str]) -> (LirFunction, TestTables) {
    let (p, tables) = checked_lowering(src, fns);
    let f = p
        .functions
        .iter()
        .find(|f| f.name.to_string() == name)
        .expect("function present in lowered IR")
        .clone();
    (f, tables)
}

fn returned_hir_expr(src: &str) -> HirExpr {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "{errs:?}");
    let (hir, diags) = crate::ir::lower::lower_program(&program);
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir.functions.first().expect("one function");
    match function.body.last().expect("return statement") {
        HirStmt::Return {
            value: Some(value), ..
        } => value.clone(),
        other => panic!("expected value return, got {other:?}"),
    }
}

#[test]
fn may_allocate_preserves_twenty_wrapper_perspectives() {
    fn node(kind: HirExprKind) -> HirExpr {
        HirExpr {
            kind,
            ty: Type::I64,
            span: Span::new(0, 0, 1, 1),
        }
    }
    fn scalar() -> Box<HirExpr> {
        Box::new(node(HirExprKind::Int(0)))
    }
    // Ten wrapper positions, each with an allocation-free scalar and an
    // allocating call. All wrappers must propagate the same conservative bit.
    for position in 0..10 {
        for allocating in [false, true] {
            let leaf = Box::new(node(if allocating {
                HirExprKind::Call {
                    callee: "value".into(),
                    args: vec![],
                }
            } else {
                HirExprKind::Int(1)
            }));
            let expr = node(match position {
                0 => HirExprKind::Unary {
                    op: UnaryOp::Neg,
                    operand: leaf,
                },
                1 => HirExprKind::ReferenceArg { place: leaf },
                2 => HirExprKind::FieldAccess {
                    object: leaf,
                    field: "field".into(),
                },
                3 => HirExprKind::Index {
                    array: leaf,
                    index: scalar(),
                },
                4 => HirExprKind::Index {
                    array: scalar(),
                    index: leaf,
                },
                5 => HirExprKind::Binary {
                    op: BinOp::Add,
                    lhs: leaf,
                    rhs: scalar(),
                },
                6 => HirExprKind::Binary {
                    op: BinOp::Add,
                    lhs: scalar(),
                    rhs: leaf,
                },
                7 => HirExprKind::Ternary {
                    condition: leaf,
                    then_expr: scalar(),
                    else_expr: scalar(),
                },
                8 => HirExprKind::Ternary {
                    condition: scalar(),
                    then_expr: leaf,
                    else_expr: scalar(),
                },
                _ => HirExprKind::Ternary {
                    condition: scalar(),
                    then_expr: scalar(),
                    else_expr: leaf,
                },
            });
            assert_eq!(may_allocate(&expr), allocating, "position={position}");
        }
    }
    for kind in [
        HirExprKind::Float(1.0),
        HirExprKind::Bool(true),
        HirExprKind::Var("x".into()),
        HirExprKind::FnRef("f".into()),
    ] {
        assert!(!may_allocate(&node(kind)));
    }
    // Even an allocation-free child does not make an unlisted wrapper safe.
    assert!(may_allocate(&node(HirExprKind::TryPropagate {
        inner: scalar()
    })));
    assert!(may_allocate(&node(HirExprKind::Str("literal".into()))));
    for closure in [false, true] {
        let mut lambda = node(HirExprKind::Lambda {
            id: crate::parser::ast::ExprId::fresh(),
            params: vec![],
            captures: vec![],
            body: vec![HirStmt::Return {
                value: Some(node(HirExprKind::Call {
                    callee: "allocates".into(),
                    args: vec![],
                })),
                span: Span::new(0, 0, 1, 1),
            }],
        });
        lambda.ty = if closure {
            Type::Closure(vec![], Box::new(Type::I64))
        } else {
            Type::Fn(vec![], Box::new(Type::I64))
        };
        assert_eq!(may_allocate(&lambda), closure);
    }
}

fn eligibility_wrapper_tables() -> TestTables {
    let tokens = Lexer::new(
        "class Box { pub value: i64; } fn pair(a: i64, b: i64) -> i64 { return a + b; }",
    )
    .tokenize()
    .unwrap();
    let (program, errors) = Parser::new(tokens).parse();
    assert!(errors.is_empty());
    TestTables::build(&program, &["pair"], &[])
}

fn eligibility_node(kind: HirExprKind, ty: Type) -> HirExpr {
    HirExpr {
        kind,
        ty,
        span: Span::new(0, 0, 1, 1),
    }
}

fn eligibility_wrapper(leaf: HirExpr, position: usize) -> HirExpr {
    let scalar = || eligibility_node(HirExprKind::Int(1), Type::I64);
    let array = |elements| {
        eligibility_node(
            HirExprKind::Array { elements },
            Type::Array(Box::new(Type::I64)),
        )
    };
    let range = |start, end| {
        eligibility_node(
            HirExprKind::Range {
                start: Box::new(start),
                end: Box::new(end),
            },
            Type::Generic("Range".into(), vec![Type::I64]),
        )
    };
    let object = |kind| eligibility_node(kind, Type::Named("Box".into()));
    match position {
        0 | 1 => eligibility_node(
            HirExprKind::Call {
                callee: "pair".into(),
                args: if position == 0 {
                    vec![leaf, scalar()]
                } else {
                    vec![scalar(), leaf]
                },
            },
            Type::I64,
        ),
        2 | 3 => eligibility_node(
            HirExprKind::Index {
                array: Box::new(array(if position == 2 {
                    vec![leaf, scalar()]
                } else {
                    vec![scalar(), leaf]
                })),
                index: Box::new(scalar()),
            },
            Type::I64,
        ),
        4 => eligibility_node(
            HirExprKind::Index {
                array: Box::new(array(vec![scalar()])),
                index: Box::new(leaf),
            },
            Type::I64,
        ),
        5 | 6 => eligibility_node(
            HirExprKind::FieldAccess {
                object: Box::new(if position == 5 {
                    range(leaf, scalar())
                } else {
                    range(scalar(), leaf)
                }),
                field: "start".into(),
            },
            Type::I64,
        ),
        7 | 8 => eligibility_node(
            HirExprKind::FieldAccess {
                object: Box::new(object(if position == 7 {
                    HirExprKind::New {
                        class: "Box".into(),
                        args: vec![leaf],
                    }
                } else {
                    HirExprKind::ObjectLiteral {
                        class: "Box".into(),
                        fields: vec![("value".into(), leaf)],
                    }
                })),
                field: "value".into(),
            },
            Type::I64,
        ),
        _ => eligibility_node(
            HirExprKind::Print {
                value: Box::new(leaf),
                newline: true,
            },
            Type::Void,
        ),
    }
}

#[test]
fn reference_names_match_ast_and_hir_for_twenty_shapes() {
    use crate::parser::ast::{Expr, ExprId};
    let span = Span::dummy();
    // Four roots times five suffix forms, including both placeholders.
    for root in 0..4 {
        for suffix in 0..5 {
            let (ast, hir, expected) = match root {
                0 => (
                    Expr::Var("root".into(), span, ExprId::fresh()),
                    HirExprKind::Var("root".into()),
                    "root",
                ),
                1 => (
                    Expr::Var("名".into(), span, ExprId::fresh()),
                    HirExprKind::Var("名".into()),
                    "名",
                ),
                2 => (
                    Expr::Integer(7, span, ExprId::fresh()),
                    HirExprKind::Int(7),
                    "<expression>",
                ),
                _ => (
                    Expr::Bool(true, span, ExprId::fresh()),
                    HirExprKind::Bool(true),
                    "<expression>",
                ),
            };
            let mut ast = Expr::FieldAccess(Box::new(ast), "field".into(), span, ExprId::fresh());
            let mut hir = eligibility_node(
                HirExprKind::FieldAccess {
                    object: Box::new(eligibility_node(hir, Type::I64)),
                    field: "field".into(),
                },
                Type::I64,
            );
            let (ast_index, hir_index, index_name) = match suffix {
                0 => (
                    Expr::Integer(0, span, ExprId::fresh()),
                    HirExprKind::Int(0),
                    "0",
                ),
                1 => (
                    Expr::Integer(-7, span, ExprId::fresh()),
                    HirExprKind::Int(-7),
                    "-7",
                ),
                2 => (
                    Expr::Var("i".into(), span, ExprId::fresh()),
                    HirExprKind::Var("i".into()),
                    "i",
                ),
                3 => (
                    Expr::Bool(true, span, ExprId::fresh()),
                    HirExprKind::Bool(true),
                    "<expr>",
                ),
                _ => (
                    Expr::Var("添字".into(), span, ExprId::fresh()),
                    HirExprKind::Var("添字".into()),
                    "添字",
                ),
            };
            ast = Expr::Index(Box::new(ast), Box::new(ast_index), span, ExprId::fresh());
            hir = eligibility_node(
                HirExprKind::Index {
                    array: Box::new(hir),
                    index: Box::new(eligibility_node(hir_index, Type::I64)),
                },
                Type::I64,
            );
            let expected = format!("{expected}.field[{index_name}]");
            assert_eq!(super::super::reference_place_name(&ast), expected);
            assert_eq!(lir_reference_place_name(&hir), expected);
        }
    }
}

#[test]
fn reference_names_use_a_one_megabyte_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            use crate::parser::ast::{Expr, ExprId};
            let span = Span::dummy();
            let mut ast = Expr::Var("root".into(), span, ExprId::fresh());
            let mut hir = eligibility_node(HirExprKind::Var("root".into()), Type::I64);
            let mut expected = String::from("root");
            for depth in 0..50_000 {
                if depth % 2 == 0 {
                    ast = Expr::FieldAccess(Box::new(ast), "x".into(), span, ExprId::fresh());
                    hir = eligibility_node(
                        HirExprKind::FieldAccess {
                            object: Box::new(hir),
                            field: "x".into(),
                        },
                        Type::I64,
                    );
                    expected.push_str(".x");
                } else {
                    ast = Expr::Index(
                        Box::new(ast),
                        Box::new(Expr::Integer(0, span, ExprId::fresh())),
                        span,
                        ExprId::fresh(),
                    );
                    hir = eligibility_node(
                        HirExprKind::Index {
                            array: Box::new(hir),
                            index: Box::new(eligibility_node(HirExprKind::Int(0), Type::I64)),
                        },
                        Type::I64,
                    );
                    expected.push_str("[0]");
                }
            }
            let ast_name = super::super::reference_place_name(&ast);
            let hir_name = lir_reference_place_name(&hir);
            drop(ast);
            drop(hir);
            assert_eq!(ast_name, expected);
            assert_eq!(hir_name, expected);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn divergence_nested_if_and_match_use_a_one_megabyte_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let span = Span::dummy();
            for diverges in [false, true] {
                let returns = || HirStmt::Return { value: None, span };
                let mut body = if diverges { vec![returns()] } else { vec![] };
                for depth in 0..50_000 {
                    body = if depth % 2 == 0 {
                        vec![HirStmt::If {
                            cond: eligibility_node(HirExprKind::Bool(true), Type::Bool),
                            then_branch: body,
                            else_branch: Some(vec![returns()]),
                            span,
                        }]
                    } else {
                        vec![HirStmt::Expr(eligibility_node(
                            HirExprKind::Match {
                                scrutinee: Box::new(eligibility_node(
                                    HirExprKind::Int(0),
                                    Type::I64,
                                )),
                                arms: vec![HirMatchArm {
                                    pattern: HirPattern::Wildcard,
                                    guard: None,
                                    body,
                                    ty: Type::Void,
                                    span,
                                }],
                            },
                            Type::Void,
                        ))]
                    };
                }
                let actual = body_diverges(&body);

                drop(body);
                assert_eq!(actual, diverges);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn eligibility_nonoperator_twenty_operand_perspectives() {
    let tables = eligibility_wrapper_tables();
    let names = HashMap::from([("value", Cow::Owned(Type::I64))]);
    // Each operand position must accept a visible local and blame the same
    // leaf when its binding is absent, through all intervening wrappers.
    for position in 0..10 {
        for known in [false, true] {
            let leaf = eligibility_node(
                HirExprKind::Var(if known { "value" } else { "missing" }.into()),
                Type::I64,
            );
            let expr = eligibility_wrapper(leaf, position);
            tables.with_ctx(|ctx| {
                assert_eq!(
                    supported_expr(&expr, ctx, &names),
                    known,
                    "position={position}"
                );
                let rejected = minimal_unsupported_expr(&expr, ctx, &names);
                if known {
                    assert!(rejected.is_none(), "position={position}");
                } else {
                    assert!(
                        matches!(&rejected.unwrap().kind, HirExprKind::Var(n) if n == "missing"),
                        "position={position}"
                    );
                }
            });
        }
    }
}

#[test]
fn eligibility_mixed_nonoperator_chains_use_a_one_megabyte_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let tables = eligibility_wrapper_tables();
            for known in [false, true] {
                let mut expr = eligibility_node(HirExprKind::Var("value".into()), Type::I64);
                for depth in 0..50_000 {
                    expr = eligibility_wrapper(expr, depth % 9);
                }
                let names = if known {
                    HashMap::from([("value", Cow::Owned(Type::I64))])
                } else {
                    HashMap::new()
                };
                let (accepted, reason) = tables.with_ctx(|ctx| {
                    (
                        supported_expr(&expr, ctx, &names),
                        expr_rejection(&expr, ctx, &names),
                    )
                });

                drop(expr);
                assert_eq!(accepted, known);
                assert_eq!(reason.is_none(), known);
                if let Some(reason) = reason {
                    assert!(reason.contains("value"));
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn eligibility_deep_scoped_match_and_select_use_one_megabyte() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let tables = eligibility_wrapper_tables();
            for select in [false, true] {
                for known in [false, true] {
                    let mut expr = eligibility_node(HirExprKind::Var("value".into()), Type::I64);
                    for _ in 0..512 {
                        let span = expr.span;
                        expr = if select {
                            eligibility_node(
                                HirExprKind::Select {
                                    cases: vec![HirSelectCase {
                                        kind: HirSelectCaseKind::Default,
                                        body: vec![HirStmt::Expr(expr)],
                                        span,
                                    }],
                                },
                                Type::Void,
                            )
                        } else {
                            eligibility_node(
                                HirExprKind::Match {
                                    scrutinee: Box::new(eligibility_node(
                                        HirExprKind::Int(1),
                                        Type::I64,
                                    )),
                                    arms: vec![HirMatchArm {
                                        pattern: HirPattern::Wildcard,
                                        guard: None,
                                        body: vec![HirStmt::Expr(expr)],
                                        ty: Type::I64,
                                        span,
                                    }],
                                },
                                Type::I64,
                            )
                        };
                    }
                    let names = if known {
                        HashMap::from([("value", Cow::Owned(Type::I64))])
                    } else {
                        HashMap::new()
                    };
                    tables.with_ctx(|ctx| {
                        assert_eq!(supported_expr(&expr, ctx, &names), known, "select={select}");
                        let reason = expr_rejection(&expr, ctx, &names);
                        assert_eq!(reason.is_none(), known, "select={select}: {reason:?}");
                    });
                    drop(expr);
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn eligibility_operator_positions_preserve_leaf_validation() {
    let program = crate::parser::ast::Program {
        instantiation_types: Vec::new(),
        type_uses: Vec::new(),
        module: None,
        imports: vec![],
        items: vec![],
    };
    let tables = TestTables::build(&program, &[], &[]);
    let span = Span::new(0, 0, 1, 1);
    let scalar = || HirExpr {
        kind: HirExprKind::Int(1),
        ty: Type::I64,
        span,
    };
    // Twenty perspectives: five operand positions times literal, bound
    // local, unknown local, and unsupported expression forms.
    for position in 0..5 {
        for leaf_case in 0..4 {
            let leaf = HirExpr {
                kind: match leaf_case {
                    0 => HirExprKind::Int(1),
                    1 => HirExprKind::Var("bound".into()),
                    2 => HirExprKind::Var("missing".into()),
                    _ => HirExprKind::FnRef("missing_function".into()),
                },
                ty: Type::I64,
                span,
            };
            let kind = match position {
                0 => HirExprKind::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(leaf),
                    rhs: Box::new(scalar()),
                },
                1 => HirExprKind::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(scalar()),
                    rhs: Box::new(leaf),
                },
                2 => HirExprKind::Unary {
                    op: UnaryOp::Neg,
                    operand: Box::new(leaf),
                },
                3 => HirExprKind::Ternary {
                    condition: Box::new(HirExpr {
                        kind: HirExprKind::Bool(true),
                        ty: Type::Bool,
                        span,
                    }),
                    then_expr: Box::new(leaf),
                    else_expr: Box::new(scalar()),
                },
                _ => HirExprKind::Ternary {
                    condition: Box::new(HirExpr {
                        kind: HirExprKind::Bool(true),
                        ty: Type::Bool,
                        span,
                    }),
                    then_expr: Box::new(scalar()),
                    else_expr: Box::new(leaf),
                },
            };
            let expr = HirExpr {
                kind,
                ty: Type::I64,
                span,
            };
            let names = HashMap::from([("bound", Cow::Owned(Type::I64))]);
            assert_eq!(
                tables.with_ctx(|ctx| supported_expr(&expr, ctx, &names)),
                leaf_case < 2,
                "position={position}, leaf={leaf_case}"
            );
            let rejected = tables.with_ctx(|ctx| minimal_unsupported_expr(&expr, ctx, &names));
            if leaf_case < 2 {
                assert!(rejected.is_none());
            } else {
                assert!(
                    matches!(
                        &rejected.unwrap().kind,
                        HirExprKind::Var(name) if name == "missing"
                    ) || matches!(
                        &rejected.unwrap().kind,
                        HirExprKind::FnRef(name) if name.is_free_named("missing_function")
                    )
                );
            }
        }
    }
}

#[test]
fn operator_rejection_preserves_first_child_and_parent_fallback() {
    let program = crate::parser::ast::Program {
        instantiation_types: Vec::new(),
        type_uses: Vec::new(),
        module: None,
        imports: vec![],
        items: vec![],
    };
    let tables = TestTables::build(&program, &[], &[]);
    let span = Span::new(0, 0, 1, 1);
    for missing in [None, Some(0), Some(1), Some(2)] {
        let child = |position, ty| HirExpr {
            kind: if missing.is_some_and(|first| position >= first) {
                HirExprKind::Var(format!("missing{position}"))
            } else if ty == Type::Bool {
                HirExprKind::Bool(true)
            } else {
                HirExprKind::Int(1)
            },
            ty,
            span,
        };
        let expr = HirExpr {
            kind: HirExprKind::Ternary {
                condition: Box::new(child(0, Type::Bool)),
                then_expr: Box::new(child(1, Type::I64)),
                else_expr: Box::new(child(2, Type::I64)),
            },
            // The parent itself fails the branch representation check.
            ty: Type::Bool,
            span,
        };
        let names = HashMap::new();
        let rejected = tables
            .with_ctx(|ctx| minimal_unsupported_expr(&expr, ctx, &names))
            .unwrap();
        if let Some(first) = missing {
            assert!(
                matches!(&rejected.kind, HirExprKind::Var(name) if name == &format!("missing{first}"))
            );
        } else {
            assert!(std::ptr::eq(rejected, &expr));
        }
    }
}

#[test]
fn eligibility_operator_chains_use_a_one_megabyte_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let program = crate::parser::ast::Program {
                instantiation_types: Vec::new(),
                type_uses: Vec::new(),
                module: None,
                imports: vec![],
                items: vec![],
            };
            let tables = TestTables::build(&program, &[], &[]);
            let span = Span::new(0, 0, 1, 1);
            for known in [false, true] {
                let mut expr = HirExpr {
                    kind: HirExprKind::Var("value".into()),
                    ty: Type::I64,
                    span,
                };
                for depth in 0..50_000 {
                    let kind = match depth % 3 {
                        0 => HirExprKind::Unary {
                            op: UnaryOp::Neg,
                            operand: Box::new(expr),
                        },
                        1 => HirExprKind::Binary {
                            op: BinOp::Add,
                            lhs: Box::new(expr),
                            rhs: Box::new(HirExpr {
                                kind: HirExprKind::Int(1),
                                ty: Type::I64,
                                span,
                            }),
                        },
                        _ => HirExprKind::Ternary {
                            condition: Box::new(HirExpr {
                                kind: HirExprKind::Bool(true),
                                ty: Type::Bool,
                                span,
                            }),
                            then_expr: Box::new(expr),
                            else_expr: Box::new(HirExpr {
                                kind: HirExprKind::Int(1),
                                ty: Type::I64,
                                span,
                            }),
                        },
                    };
                    expr = HirExpr {
                        kind,
                        ty: Type::I64,
                        span,
                    };
                }
                let names = if known {
                    HashMap::from([("value", Cow::Owned(Type::I64))])
                } else {
                    HashMap::new()
                };
                let accepted = tables.with_ctx(|ctx| supported_expr(&expr, ctx, &names));
                let rejection = tables.with_ctx(|ctx| expr_rejection(&expr, ctx, &names));

                drop(expr);
                assert_eq!(accepted, known);
                if known {
                    assert!(rejection.is_none());
                } else {
                    assert!(rejection.unwrap().contains("value"));
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn may_allocate_uses_a_one_megabyte_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let span = Span::new(0, 0, 1, 1);
            let mut expr = HirExpr {
                kind: HirExprKind::Int(0),
                ty: Type::I64,
                span,
            };
            for _ in 0..50_000 {
                expr = HirExpr {
                    kind: HirExprKind::Unary {
                        op: UnaryOp::Neg,
                        operand: Box::new(expr),
                    },
                    ty: Type::I64,
                    span,
                };
            }
            let allocates = may_allocate(&expr);
            drop(expr);
            assert!(!allocates);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn may_allocate_distinguishes_string_comparison_from_concatenation() {
    let eq = returned_hir_expr("fn f(a: String, b: String) -> bool { return a == b; }");
    let ne = returned_hir_expr("fn f(a: String, b: String) -> bool { return a != b; }");
    for op in ["<", "<=", ">", ">="] {
        let expr = returned_hir_expr(&format!(
            "fn f(a: String, b: String) -> bool {{ return a {op} b; }}"
        ));
        assert!(!may_allocate(&expr), "{op} must not allocate");
    }
    let concat = returned_hir_expr("fn f(a: String, b: String) -> String { return a + b; }");

    assert!(
        !may_allocate(&eq) && !may_allocate(&ne),
        "willow_string_eq is an allocation-free byte comparison"
    );
    assert!(
        may_allocate(&concat),
        "willow_string_concat allocates its result"
    );
}

// 1. a scalar arithmetic function is eligible
#[test]
fn e01_scalar_fn_eligible() {
    assert!(eligible(
        "fn add(a: i64, b: i64) -> i64 { return a + b; }",
        "add",
        &["add"]
    ));
}

// 2. recursive control flow (fib) is eligible
#[test]
fn e02_fib_eligible() {
    let src = "fn fib(n: i64) -> i64 { if n <= 1 { return n; } return fib(n-1) + fib(n-2); }";
    assert!(eligible(src, "fib", &["fib"]));
}

// 3. print of a scalar is eligible
#[test]
fn e03_scalar_print_eligible() {
    assert!(eligible(
        "fn show(n: i64) { println(n * 2); }",
        "show",
        &["show"]
    ));
}

// 4. string values became eligible with GC
// rooting; kept as a positive check so a regression here is loud.
#[test]
fn e04_string_now_eligible() {
    assert!(eligible("fn s() { println(\"hi\"); }", "s", &["s"]));
}

// 5. (updated) short-circuit operators became eligible with lazy block
// emission; kept as a positive check so a regression here is loud.
#[test]
fn e05_short_circuit_now_eligible() {
    assert!(eligible(
        "fn f(a: bool, b: bool) -> bool { return a && b; }",
        "f",
        &["f"]
    ));
}

// 6. unknown callees are not eligible
#[test]
fn e06_unknown_callee_ineligible() {
    assert!(!eligible(
        "fn g() -> i64 { println(0); return 1; } fn f() -> i64 { return g(); }",
        "f",
        &[] // g not in the known set
    ));
}

// 7. shadowing a `let` in an inner scope is
// now eligible: HIR lowering α-renames the second binding, so the flat LIR
// namespace never sees two `x` slots.
#[test]
fn e07_shadowing_eligible() {
    let src = "fn f(c: bool) -> i64 { let x = 1; if c { let x = 2; print(x); } return x; }";
    assert!(eligible(src, "f", &["f"]));
}

// 8. while/for loops stay eligible (control flow is blocks, not exprs)
#[test]
fn e08_loops_eligible() {
    let src = "fn sum_to(n: i64) -> i64 { let mut t = 0; for i in 0..n { t = t + i; } return t; }";
    assert!(eligible(src, "sum_to", &["sum_to"]));
}

// 9. array-typed values are now eligible
#[test]
fn e09_arrays_now_eligible() {
    let src = "fn f() -> i64 { let xs = [1, 2]; return xs.len(); }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// 10. f64 arithmetic + comparison is eligible
#[test]
fn e10_f64_eligible() {
    let src = "fn half(x: f64) -> bool { return x / 2.0 > 1.0; }";
    assert!(eligible(src, "half", &["half"]));
}

// 11. both shared and mutable reference parameters use the pointer ABI and
// are ordinary eligible LIR functions.
#[test]
fn e11_reference_params_eligible_via_hir() {
    let src = "fn bump(n: &mut i64) { n = n + 1; }";
    assert!(eligible(src, "bump", &["bump"]));
    let src2 = "fn read(n: &i64) -> i64 { return n; }";
    assert!(eligible(src2, "read", &["read"]));
}

// 12. short-circuit && / || are now eligible (lazy block emission)
#[test]
fn e12_short_circuit_eligible() {
    assert!(eligible(
        "fn f(a: bool, b: bool) -> bool { return a && b || !a; }",
        "f",
        &["f"]
    ));
}

// 13. scalar ternaries are eligible
#[test]
fn e13_ternary_eligible() {
    assert!(eligible(
        "fn f(c: bool) -> i64 { return c ? 1 : 2; }",
        "f",
        &["f"]
    ));
}

// 14. a String ternary is now eligible
#[test]
fn e14_string_ternary_now_eligible() {
    let src = "fn f(c: bool) -> String { let s = c ? \"a\" : \"b\"; return s; }";
    assert!(eligible(src, "f", &["f"]));
}

// ---------------------------------------------------------------------
// — GC-managed values and rooting in the LIR walker.
//
// Perspectives 1-12 below are the *eligibility* half (which functions the
// LIR path claims); perspectives 13-32 live in `tests/integration` as
// differential and GC-stress runs, because they are about emitted code, not
// about the predicate.
//
//  1. a String parameter/return function is eligible
//  2. String concatenation is eligible
//  3. String equality/inequality is eligible
//  4. `println` of a String is eligible
//  5. a String ternary is eligible
//  6. mixed scalar + String locals in one function are eligible
//  7. a String `let` that is reassigned in a loop is eligible
//  8. calling a String-returning function is eligible
//  9. a `let` shadowing a PARAMETER is rejected (flattened scopes)
// 10. enum variants need resolved enum metadata
// 11. an unsupported String operator (`<`) is rejected
// 12. array, class, and interface representations are checked separately
// ---------------------------------------------------------------------

// 15. String parameters and returns are eligible
#[test]
fn e15_string_param_and_return_eligible() {
    let src = "fn id(s: String) -> String { return s; }";
    assert!(eligible(src, "id", &["id"]));
}

// 16. concatenation of strings is eligible
#[test]
fn e16_string_concat_eligible() {
    let src = "fn join(a: String, b: String) -> String { return a + b; }";
    assert!(eligible(src, "join", &["join"]));
}

// 17. string equality and inequality are eligible
#[test]
fn e17_string_compare_eligible() {
    let eq = "fn f(a: String, b: String) -> bool { return a == b; }";
    assert!(eligible(eq, "f", &["f"]));
    let ne = "fn f(a: String, b: String) -> bool { return a != b; }";
    assert!(eligible(ne, "f", &["f"]));
}

// 18. a String local reassigned inside a loop is eligible — the case the
// entry-rooted slot design exists for (a per-`let` root would grow the
// shadow stack once per iteration).
#[test]
fn e18_string_loop_accumulator_eligible() {
    let src = "fn rep(n: i64) -> String { let mut s = \"\"; let mut i = 0; \
                   while i < n { s = s + \"x\"; i = i + 1; } return s; }";
    assert!(eligible(src, "rep", &["rep"]));
}

// 19. mixed scalar and String locals in one function are eligible
#[test]
fn e19_mixed_scalar_and_gc_eligible() {
    let src = "fn f(n: i64) -> String { let tag = \"n=\"; let doubled = n * 2; \
                   let ok = doubled > 0; return ok ? tag : \"\"; }";
    assert!(eligible(src, "f", &["f"]));
}

// 20. a call that both takes and returns a String is eligible
#[test]
fn e20_string_call_eligible() {
    let src = "fn wrap(s: String) -> String { return \"[\" + s + \"]\"; } \
                   fn f() -> String { return wrap(\"a\"); }";
    assert!(eligible(src, "f", &["f", "wrap"]));
}

// 21. a `let` shadowing a PARAMETER is now
// eligible for the same reason as e07 — the `let` is renamed, so the
// parameter's storage keeps the source name to itself.
#[test]
fn e21_let_shadowing_param_eligible() {
    let src = "fn f(s: String) -> String { let s = \"other\"; return s; }";
    assert!(eligible(src, "f", &["f"]));
}

// 22. the QUALIFIED variant form is now emitted,
// so it is eligible. The bare form is still checked here because it does not
// survive HIR lowering at all: without that guard a future lowering change
// could quietly hand the walker a `Var` it would resolve to a local (the
// `names` guard in `lir_supported_function` is the backstop).
#[test]
fn e22_bare_enum_variant_never_reaches_walker() {
    let bare = "enum Status { Open, Closed } fn f() -> Status { return Closed; }";
    let tokens = Lexer::new(bare).tokenize().expect("lex");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "{errs:?}");
    let (_, diags) = crate::ir::lower::lower_program(&program);
    assert!(!diags.is_empty(), "bare variant unexpectedly lowered");

    let qualified = "enum Status { Open, Closed } fn f() -> Status { return Status::Closed; }";
    assert!(eligible_checked(qualified, "f", &["f"]));
}

// 23. lexicographic string ordering lowers to `willow_string_cmp`, so
// every ordering operator on two strings is eligible.
#[test]
fn e23_string_ordering_eligible() {
    for op in ["<", "<=", ">", ">="] {
        let src = format!("fn f(a: String, b: String) -> bool {{ return a {op} b; }}");
        assert!(eligible_checked(&src, "f", &["f"]), "{op}");
    }
}

// 24. arrays of strings are now eligible
#[test]
fn e24_string_array_now_eligible() {
    let src = "fn f() -> i64 { let xs = [\"a\", \"b\"]; return xs.len(); }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// 25. a static call returning a class object is
// now claimed by the LIR path
#[test]
fn e25_class_object_now_eligible() {
    let src = "class Item { name: String; pub static fn make(n: String) -> Item \
                   { return new Item(n); } } \
                   fn f() -> Item { return Item::make(\"a\"); }";
    assert!(eligible_lenient(src, "f", &["f"]));
}

// ---------------------------------------------------------------------
// — `Array<T>` in the LIR walker.
//
// Perspectives 1-15 below are the *eligibility* half (which functions the
// LIR path claims); perspectives 16-38 live in `tests/integration` as
// differential and `WILLOW_GC_STRESS=alloc` runs, because they are about
// emitted code and rooting, not about the predicate.
//
//  1. an array literal + `len()` is eligible
//  2. array parameters and array returns are eligible
//  3. indexing an array is eligible
//  4. index-assignment (`a[i] = v`) is eligible
//  5. `push` / `pop` are eligible
//  6. `toString()` is eligible for every renderable element kind
//  7. an array of `String` is eligible (GC element type)
//  8. `Array<Array<i64>>` is eligible (element type checked recursively)
//  9. an empty array literal never reaches the predicate: HIR lowering
//     rejects it before the walker sees the function
// 10. `for x in arr` is eligible (desugars to `len`/index)
// 11. an array of class objects is rejected (no interface boxing here)
// 12. an unsupported array method (`freeze`) is rejected
// 13. a `FrozenArray<T>` receiver is rejected (different runtime call)
// 14. a `Map<K, V>` receiver/index is rejected
// 15. `toString()` on a non-renderable element type is rejected
// ---------------------------------------------------------------------

// 26. array parameters and array returns are eligible
#[test]
fn e26_array_param_and_return_eligible() {
    let src = "import std::collections::Array; fn f(xs: Array<i64>) -> Array<i64> { return xs; }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// 27. reading through an index is eligible
#[test]
fn e27_array_index_eligible() {
    let src =
        "import std::collections::Array; fn f(xs: Array<i64>, i: i64) -> i64 { return xs[i]; }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// 28. index-assignment is eligible (a LIR instruction, not an expression)
#[test]
fn e28_index_assign_eligible() {
    let src = "fn f() -> i64 { let mut xs = [1, 2]; xs[0] = 9; return xs[0]; }";
    assert!(eligible(src, "f", &["f"]));
}

// 29. push and pop are eligible
#[test]
fn e29_push_pop_eligible() {
    let src = "fn f() -> i64 { let mut xs = [1]; xs.push(2); return xs.pop(); }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// 30. toString is eligible for each element kind the runtime can render
#[test]
fn e30_to_string_eligible_for_scalar_kinds() {
    for (decl, lit) in [
        ("i64", "[1, 2]"),
        ("f64", "[1.5]"),
        ("bool", "[true]"),
        ("String", "[\"a\"]"),
    ] {
        let src = format!(
            "import std::collections::Array; fn f() -> String {{ let xs: Array<{decl}> = {lit}; return xs.toString(); }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]), "{decl} array toString");
    }
}

// 31. an `Array<Array<i64>>` is eligible: the element type is itself checked
#[test]
fn e31_nested_array_eligible() {
    let src = "fn f() -> i64 { let xs = [[1, 2], [3]]; return xs[0][1]; }";
    assert!(eligible(src, "f", &["f"]));
}

// 32. an annotated empty literal reaches the
// walker with the element type recorded by the checker.
#[test]
fn e32_annotated_empty_array_eligible() {
    let src = "import std::collections::Array; \
                   fn f() -> i64 { let mut xs: Array<i64> = []; xs.push(1); return xs.len(); }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// 33. `for x in arr` is eligible: the LIR desugars it into `len()` + index
#[test]
fn e33_for_over_array_eligible() {
    let src = "import std::collections::Array; fn f(xs: Array<i64>) -> i64 { let mut t = 0; for x in xs { t = t + x; } return t; }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// 34. arrays of SIMPLE class objects are eligible now (the
// element is a plain GC handle); an array of an INTERFACE element type
// joined them in, once the walker learned to box on the way in.
#[test]
fn e34_class_and_interface_element_arrays_eligible() {
    let src = "import std::collections::Array; class Item { pub name: String; } \
                   fn f(xs: Array<Item>) -> i64 { return xs.len(); }";
    assert!(eligible_checked(src, "f", &["f"]));

    let iface = "import std::collections::Array; interface Named { fn name(self) -> String; } \
                     fn f(xs: Array<Named>) -> i64 { return xs.len(); }";
    assert!(eligible_checked(iface, "f", &["f"]));
}

// 35. `freeze` completed the array method
// surface — `len`/`push`/`pop`/`toString`/`freeze` is all of it, so there is
// no longer an unsupported array method. Kept as a positive check.
#[test]
fn e35_array_freeze_now_eligible() {
    let src = "fn f() -> i64 { let xs = [1, 2]; let ys = xs.freeze(); return ys.len(); }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// 36. a `Map<K, V>` receiver is not an ARRAY
// receiver, but it is now a receiver the walker claims in its own right.
#[test]
fn e36_map_now_eligible() {
    let src = "import std::collections::Map; \
                   fn f() -> i64 { let m: Map<String, i64> = Map::new(); \
                   m.insert(\"a\", 1); return m.len(); }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// 37. `toString()` on an element type the runtime cannot render is rejected
#[test]
fn e37_nested_array_to_string_ineligible() {
    let src = "fn f() -> String { let xs = [[1], [2]]; return xs.toString(); }";
    assert!(!eligible_lenient(src, "f", &["f"]));
}

// ---------------------------------------------------------------------
// — remaining one-off LIR blockers.
//
// Perspectives 1-20 make the four acceptance surfaces explicit:
//  1. a Range<i64> literal may be returned as a value
//  2. Range<i64> may cross a parameter boundary
//  3. Range.start and Range.end are eligible field reads
//  4. a for loop may consume a stored Range value
//  5. a for loop may consume a call-returned Range value
//  6. an annotated empty Array<i64> literal is eligible
//  7. an annotated empty Array<String> literal is eligible
//  8. an annotated empty nested Array literal is eligible
//  9. push after empty construction remains eligible
// 10. an empty array may be returned from a function
// 11. sibling for loops may reuse the same binding name
// 12. a nested block may shadow an outer binding
// 13. a let binding may shadow a parameter
// 14. three sibling loops receive three distinct HIR names
// 15. renamed bindings preserve the outer value after the inner scope
// 16. void main(args: Array<String>) is eligible
// 17. main may index its args array
// 18. env::args() is eligible on the same path
// 19. env::args_len() is eligible on the same path
// 20. env::program_name() is eligible on the same path
// ---------------------------------------------------------------------

#[test]
fn one_off_01_range_values_are_eligible() {
    let cases = [
        "fn f() -> Range<i64> { return 1..4; }",
        "fn id(r: Range<i64>) -> Range<i64> { return r; }",
        "fn f(r: Range<i64>) -> i64 { return r.end - r.start; }",
        "fn f(r: Range<i64>) -> i64 { let mut n = 0; for x in r { n = n + x; } return n; }",
        "fn make() -> Range<i64> { return 0..3; } fn f() -> i64 { let mut n = 0; for x in make() { n = n + x; } return n; }",
    ];
    for src in cases {
        let name = if src.starts_with("fn id") { "id" } else { "f" };
        assert!(eligible_checked(src, name, &["make", "id", "f"]), "{src}");
    }
}

#[test]
fn one_off_02_context_typed_empty_arrays_are_eligible() {
    let cases = [
        "import std::collections::Array; fn f() -> i64 { let xs: Array<i64> = []; return xs.len(); }",
        "import std::collections::Array; fn f() -> i64 { let xs: Array<String> = []; return xs.len(); }",
        "import std::collections::Array; fn f() -> i64 { let xs: Array<Array<i64>> = []; return xs.len(); }",
        "import std::collections::Array; fn f() -> i64 { let xs: Array<i64> = []; xs.push(7); return xs[0]; }",
        "import std::collections::Array; fn f() -> Array<i64> { let xs: Array<i64> = []; return xs; }",
    ];
    for src in cases {
        assert!(eligible_checked(src, "f", &["f"]), "{src}");
    }
}

#[test]
fn one_off_03_shadowed_bindings_are_alpha_renamed() {
    let cases = [
        "fn f() -> i64 { let xs = [1]; let mut n = 0; for x in xs { n = n + x; } for x in xs { n = n + x; } return n; }",
        "fn f(c: bool) -> i64 { let x = 1; if c { let x = 2; print(x); } return x; }",
        "fn f(x: i64) -> i64 { let x = x + 1; return x; }",
        "fn f() -> i64 { let xs = [1]; let mut n = 0; for x in xs { n = n + x; } for x in xs { n = n + x; } for x in xs { n = n + x; } return n; }",
        "fn f(c: bool) -> i64 { let x = 7; if c { let x = 9; print(x); } return x; }",
    ];
    for src in cases {
        assert!(eligible_checked(src, "f", &["f"]), "{src}");
    }
}

#[test]
fn one_off_04_main_args_and_env_calls_are_eligible() {
    let cases = [
        "import std::collections::Array; fn main(args: Array<String>) { println(args.len()); }",
        "import std::collections::Array; fn main(args: Array<String>) { if args.len() > 0 { println(args[0]); } }",
        "import std::collections::Array; fn main(args: Array<String>) { let xs = env::args(); println(xs.len()); }",
        "import std::collections::Array; fn main(args: Array<String>) { println(env::args_len()); }",
        "import std::collections::Array; fn main(args: Array<String>) { println(env::program_name()); }",
    ];
    for src in cases {
        assert!(eligible_checked(src, "main", &["main"]), "{src}");
    }
}

// ---------------------------------------------------------------------
// — a builtin namespace reached through an `import` alias.
//
// Declaration normalization folds aliases with `normalize_std_collection_program`.
// The walker lowers to HIR from the
// RAW frontend program, so it does, and resolves the alias at the point of
// dispatch — before the gate that lets a user module of the same name win,
// which is the order those two passes apply between them.
//
//  na1 every aliasable namespace is admitted through its alias
//  na2 an alias nobody declared is not invented
//  na3 a self-alias resolves to itself
//  na4 the alias reaches the same table entry as the canonical name
//  na5 a user module of the canonical name still wins
//  na6 `f64::` is answered before the alias map

#[test]
fn na1_aliased_namespace_calls_are_eligible() {
    let cases = [
        "import std::fs as files; fn f(p: String) -> bool { return files::exists(p); }",
        "import std::env as sys; fn f() -> i64 { return sys::args_len(); }",
        "import std::env as sys; fn f() -> String { return sys::program_name(); }",
        "import std::net as sock; fn f(a: String) -> Result<TcpListener, IoError> { return sock::bind(a); }",
    ];
    for src in cases {
        assert!(eligible_checked(src, "f", &["f"]), "{src}");
    }
}

#[test]
fn na2_an_undeclared_alias_is_not_a_namespace() {
    // No `import` records `files`, so the call is an ordinary static call
    // to a module that does not exist. Admitting it would mean emitting a
    // runtime call the program never asked for.
    let src = "fn f(p: String) -> bool { return files::exists(p); }";
    assert!(!eligible_lenient(src, "f", &["f"]), "{src}");
}

#[test]
fn na3_a_self_alias_resolves_to_itself() {
    let src = "import std::fs as fs; fn f(p: String) -> bool { return fs::exists(p); }";
    assert!(eligible_checked(src, "f", &["f"]), "{src}");
}

#[test]
fn na4_an_alias_reaches_the_same_entry_as_the_canonical_name() {
    let mut aliases = HashMap::new();
    aliases.insert("files".to_string(), "fs".to_string());
    aliases.insert("par".to_string(), "parallel".to_string());
    let empty = HashMap::new();
    for (alias, canonical, method) in [
        ("files", "fs", "exists"),
        ("files", "fs", "read_to_string"),
        ("par", "parallel", "map"),
    ] {
        let through = namespace_builtin_call(&ModuleSymbols::default(), &aliases, alias, method)
            .unwrap_or_else(|| panic!("{alias}::{method} has no entry"));
        let direct = namespace_builtin_call(&ModuleSymbols::default(), &empty, canonical, method)
            .unwrap_or_else(|| panic!("{canonical}::{method} has no entry"));
        assert_eq!(through.runtime, direct.runtime, "{alias}::{method}");
        assert_eq!(through.params, direct.params, "{alias}::{method}");
        assert_eq!(through.ret, direct.ret, "{alias}::{method}");
        assert_eq!(
            through.narrow_to_bool, direct.narrow_to_bool,
            "{alias}::{method}"
        );
    }
}

#[test]
fn na5_a_user_module_wins_only_through_its_access_name() {
    // `files` and `fs` are distinct access paths even though the std alias
    // maps to the same canonical namespace name.
    let mut known = ModuleSymbols::default();
    known.insert("fs".to_string(), "fs__".to_string());
    let mut aliases = HashMap::new();
    aliases.insert("files".to_string(), "fs".to_string());
    assert!(namespace_builtin_call(&known, &aliases, "files", "exists").is_some());
    assert!(namespace_builtin_call(&known, &aliases, "fs", "exists").is_none());
    // Without the user module the same call IS the builtin.
    assert!(
        namespace_builtin_call(&ModuleSymbols::default(), &aliases, "files", "exists").is_some()
    );
}

#[test]
fn na6_f64_is_answered_before_the_alias_map() {
    // `f64::` reaches no module and is not in the stdlib schema, so it is
    // answered ahead of everything an `import` could have renamed.
    let mut aliases = HashMap::new();
    aliases.insert("f64".to_string(), "fs".to_string());
    let entry = namespace_builtin_call(&ModuleSymbols::default(), &aliases, "f64", "to_string")
        .expect("f64::to_string has an entry");
    assert_eq!(entry.runtime, "willow_f64_to_string");
    assert!(namespace_builtin_call(&ModuleSymbols::default(), &aliases, "f64", "exists").is_none());
}

// ---------------------------------------------------------------------
// — a bare module class name resolves against the modules the
// unit being compiled can SEE, and only then against every module the
// build declared.
//
// Perspectives 1-10:
//  1. a name the unit's own tables answer is never re-resolved
//  2. the one visible module that declares it answers
//  3. an unrelated INVISIBLE module of the same name no longer blocks it
//  4. two VISIBLE modules with different classes of that name: ambiguous
//  5. two spellings of ONE class (same type_id) are one answer
//  6. nothing visible declares it: the all-modules scan still answers
//  7. nothing visible, and the invisible ones disagree: ambiguous
//  8. a class no module declares at all
//  9. a candidate with no type_id is never merged by name
// 10. a visible name that is not a module is ignored

/// The two tables [`resolve_class_key`] reads: layouts by class key, and
/// the runtime type id of each (a negative id in the fixture means the
/// class has none).
struct ResolveTables {
    layouts: TypeMap<'static, std::sync::Arc<Vec<(String, Type)>>>,
    ids: TypeMap<'static, i64>,
}

impl ClassTables for ResolveTables {
    fn class_fields(&self, class: &TypeId) -> Option<std::sync::Arc<Vec<(String, Type)>>> {
        self.layouts.get(class).cloned()
    }

    fn class_type_id(&self, class: &TypeId) -> Option<i64> {
        self.ids.get(class).copied()
    }

    fn class_base(&self, _class: &TypeId) -> Option<TypeId> {
        None
    }
}

/// Class layouts and runtime ids holding one empty class per entry.
fn class_tables(classes: &[(&str, i64)]) -> ResolveTables {
    let mut layouts = TypeMap::new();
    let mut ids = TypeMap::new();
    for (name, id) in classes {
        layouts.insert((*name).to_string(), Vec::new().into());
        if *id >= 0 {
            ids.insert((*name).to_string(), *id);
        }
    }
    ResolveTables { layouts, ids }
}

fn modules(names: &[&str]) -> ModuleSymbols<'static> {
    names
        .iter()
        .map(|n| ((*n).to_string(), format!("{n}.")))
        .collect()
}

fn visible(names: &[&str]) -> HashSet<String> {
    names.iter().map(|n| (*n).to_string()).collect()
}

#[test]
fn vm1_an_own_class_is_never_re_resolved() {
    let tables = class_tables(&[("Point", 1), ("a::Point", 2)]);
    assert_eq!(
        resolve_class_key(&tables, &modules(&["a"]), &visible(&["a"]), "Point"),
        Some(TypeId::from_source_name("Point"))
    );
}

#[test]
fn vm2_the_visible_module_answers() {
    let tables = class_tables(&[("a::Point", 1)]);
    assert_eq!(
        resolve_class_key(&tables, &modules(&["a"]), &visible(&["a"]), "Point"),
        Some(TypeId::from_source_name("a::Point"))
    );
}

#[test]
fn vm3_an_unimported_module_of_the_same_name_does_not_block() {
    // The bug this fixes: `b` is a module the entry never imported, and its
    // unrelated `Point` used to make the name ambiguous and cost the body
    // its lowering.
    let tables = class_tables(&[("a::Point", 1), ("b::Point", 2)]);
    assert_eq!(
        resolve_class_key(&tables, &modules(&["a", "b"]), &visible(&["a"]), "Point"),
        Some(TypeId::from_source_name("a::Point"))
    );
}

#[test]
fn vm4_two_visible_modules_of_the_same_name_are_ambiguous() {
    // Both are in scope at this site, so nothing here can say which layout
    // the name means: refuse, rather than pick one.
    let tables = class_tables(&[("a::Point", 1), ("b::Point", 2)]);
    assert_eq!(
        resolve_class_key(
            &tables,
            &modules(&["a", "b"]),
            &visible(&["a", "b"]),
            "Point"
        ),
        None
    );
}

#[test]
fn vm5_two_spellings_of_one_class_are_one_answer() {
    // One module imported under two access names is one runtime class, so
    // the shared type_id makes the two keys agree.
    let tables = class_tables(&[("c::Point", 7), ("checks::Point", 7)]);
    let key = resolve_class_key(
        &tables,
        &modules(&["c", "checks"]),
        &visible(&["c", "checks"]),
        "Point",
    )
    .expect("one class, two spellings");
    assert!(
        key == "c::Point".into() || key == "checks::Point".into(),
        "{key}"
    );
}

#[test]
fn vm6_an_invisible_module_still_answers_when_nothing_visible_does() {
    // A class reached through a module only ANOTHER module imports keeps
    // resolving: the visible pass is a preference, not a filter.
    let tables = class_tables(&[("deep::Point", 1)]);
    assert_eq!(
        resolve_class_key(
            &tables,
            &modules(&["deep"]),
            &visible(&["shallow"]),
            "Point"
        ),
        Some(TypeId::from_source_name("deep::Point"))
    );
}

#[test]
fn vm7_invisible_modules_that_disagree_are_still_ambiguous() {
    let tables = class_tables(&[("a::Point", 1), ("b::Point", 2)]);
    assert_eq!(
        resolve_class_key(&tables, &modules(&["a", "b"]), &HashSet::new(), "Point"),
        None
    );
}

#[test]
fn vm8_a_class_no_module_declares_resolves_to_nothing() {
    let tables = class_tables(&[("a::Point", 1)]);
    assert_eq!(
        resolve_class_key(&tables, &modules(&["a"]), &visible(&["a"]), "Rect"),
        None
    );
}

#[test]
fn vm9_a_candidate_without_a_type_id_is_never_merged() {
    // No id means no proof the two names are one class, so they cannot be
    // merged even when they are both visible.
    let tables = class_tables(&[("a::Point", -1), ("b::Point", -1)]);
    assert_eq!(
        resolve_class_key(
            &tables,
            &modules(&["a", "b"]),
            &visible(&["a", "b"]),
            "Point"
        ),
        None
    );
}

#[test]
fn vm10_a_visible_name_that_is_not_a_module_is_ignored() {
    // A unit's visible set can hold names that are not modules of this
    // build at all; only the ones `known_modules` knows select a candidate.
    let tables = class_tables(&[("a::Point", 1)]);
    assert_eq!(
        resolve_class_key(
            &tables,
            &modules(&["a"]),
            &visible(&["fs", "Point", "a"]),
            "Point"
        ),
        Some(TypeId::from_source_name("a::Point"))
    );
}

// ---------------------------------------------------------------------
// — static-property reads, stores and initialization.
//
// Perspectives 1-20:
//  1. read an i64 static
//  2. assign an i64 static mut
//  3. update a static relative to its previous value
//  4. assign a bool static mut
//  5. assign an f64 static mut
//  6. assign a String static mut
//  7. a static method assigns its class static
//  8. an instance method assigns its class static
//  9. a subclass-qualified store resolves inherited storage
// 10. a String RHS may allocate before the global store
// 11. a call result may be stored
// 12. a store in a while loop is eligible
// 13. a store in an if branch is eligible
// 14. repeated stores in one function are eligible
// 15. two static properties keep distinct storage
// 16. a static initializer may read an earlier static of the same class
// 17. a chain of same-class initializers preserves declaration order
// 18. a later class initializer may read an earlier class's static
// 19. an Array static mut may be replaced
// 20. an Option static mut may be replaced
//
// Initializers themselves are emitted by the shared `__willow_static_init`
// path, in `static_init_order`; perspectives 16-18 prove the functions that
// consume those initialized slots remain eligible. The forced-LIR runnable
// examples prove their runtime order and output end to end.
// ---------------------------------------------------------------------

#[test]
fn static_lir_01_scalar_and_gc_stores_are_eligible() {
    let cases = [
        "class S { pub static n: i64 = 1; } fn f() -> i64 { return S::n; }",
        "class S { pub static mut n: i64 = 1; } fn f() { S::n = 2; }",
        "class S { pub static mut n: i64 = 1; } fn f() { S::n = S::n + 1; }",
        "class S { pub static mut b: bool = false; } fn f() { S::b = true; }",
        "class S { pub static mut x: f64 = 1.0; } fn f() { S::x = 2.5; }",
        "class S { pub static mut s: String = \"a\"; } fn f() { S::s = \"b\"; }",
    ];
    for src in cases {
        assert!(eligible_checked(src, "f", &["f"]), "{src}");
    }
}

#[test]
fn static_lir_02_method_and_inherited_stores_are_eligible() {
    let cases = [
        (
            "class S { pub static mut n: i64 = 0; pub static fn bump() { S::n = S::n + 1; } }",
            "S::bump",
        ),
        (
            "class S { pub static mut n: i64 = 0; pub fn bump(self) { S::n = S::n + 1; } }",
            "S::bump",
        ),
        (
            "open class Base { pub static mut n: i64 = 0; } class Child extends Base {} fn f() { Child::n = 3; }",
            "f",
        ),
        (
            "class S { pub static mut s: String = \"a\"; } fn f() { S::s = S::s + \"b\"; }",
            "f",
        ),
        (
            "class S { pub static mut n: i64 = 0; } fn value() -> i64 { return 7; } fn f() { S::n = value(); }",
            "f",
        ),
    ];
    for (src, name) in cases {
        assert!(eligible_checked(src, name, &["f", "value"]), "{src}");
    }
}

#[test]
fn static_lir_03_control_flow_and_distinct_slots_are_eligible() {
    let cases = [
        "class S { pub static mut n: i64 = 0; } fn f() { let mut i = 0; while i < 3 { S::n = i; i = i + 1; } }",
        "class S { pub static mut n: i64 = 0; } fn f(c: bool) { if c { S::n = 1; } }",
        "class S { pub static mut n: i64 = 0; } fn f() { S::n = 1; S::n = 2; }",
        "class S { pub static mut a: i64 = 0; pub static mut b: i64 = 0; } fn f() { S::a = 1; S::b = 2; }",
    ];
    for src in cases {
        assert!(eligible_checked(src, "f", &["f"]), "{src}");
    }
}

#[test]
fn static_lir_04_initializer_order_and_composite_slots_are_eligible() {
    let cases = [
        "class S { pub static a: i64 = 2; pub static b: i64 = S::a + 3; } fn f() -> i64 { return S::b; }",
        "class S { pub static a: i64 = 2; pub static b: i64 = S::a + 3; pub static c: i64 = S::b + 4; } fn f() -> i64 { return S::c; }",
        "class A { pub static n: i64 = 2; } class B { pub static n: i64 = A::n + 3; } fn f() -> i64 { return B::n; }",
        "import std::collections::Array; class S { pub static mut xs: Array<i64> = [1]; } fn f() { S::xs = [2, 3]; }",
        "class S { pub static mut x: Option<i64> = Option::None; } fn f() { S::x = Option::Some(7); }",
    ];
    for src in cases {
        assert!(eligible_checked(src, "f", &["f"]), "{src}");
    }
}

// ---------------------------------------------------------------------
// — class objects and field access in the LIR walker.
//
// This block began with the original "simple class" subset. The current
// walker also claims inherited layouts and virtual dispatch (2.4),
// interface boxing/dispatch, and enum payloads; these tests remain the
// baseline for direct class layout and field operations.
//
// Perspectives 1-26 below are the *eligibility* half; perspectives 27-46
// live in `tests/integration/codegen.rs` as differential and
// `WILLOW_GC_STRESS=alloc` runs, because they are about the emitted code
// and its GC rooting, not about the predicate.
//
//  1. a class-typed parameter and return is eligible
//  2. `new C(..)` through the implicit memberwise constructor is eligible
//  3. `new C(..)` through an explicit `init` is eligible
//  4. an object literal `C { f: v }` is eligible
//  5. an object literal missing a declared field is not claimed
//  6. reading a field is eligible
//  7. assigning a field is eligible
//  8. a chained field read (`a.b.c`) is eligible
//  9. an instance method call is eligible
// 10. a static method call is eligible
// 11. a GC-managed (`String`) field is eligible
// 12. an `Array<T>` field is eligible
// 13. a class local declared before a `while` keeps its entry root slot
// 14. a subclass (`extends`) is rejected — virtual dispatch
// 15. a base class (something extends it) is rejected — callers may be
//     holding a subclass instance whose layout differs
// 16. an interface-typed field (boxing flipped this to eligible: the
//     store into it boxes)
// 17. an enum-typed field is rejected
// 18. DISPATCHING through an interface-typed parameter is rejected
// 19. `let x: Iface = new C();` followed by a dispatch is rejected — the
//     BINDING type widens, which  made emittable, but the
//     virtual call on it is still out of subset
// 20. a method with a `&mut` parameter is rejected (mode check)
// 21. dispatching on the interface a method returned is rejected
// 22. a self-referential field type is eligible — the support check is
//     cycle-safe and must not recurse forever
// 23. an array of simple class objects with a field read is eligible
// 24. an OPTIONAL class type (`Node?` = `Option<Node>`) is rejected
//     everywhere it appears because generic enums remain outside this stage
// 25. a base class reached under an IMPORT ALIAS is still rejected: class
//     identity is the runtime `type_id`, not the name
// 26. an object literal naming the same field twice is rejected, not just
//     one with the wrong field COUNT
// ---------------------------------------------------------------------

/// A minimal simple class, reused by the perspectives below.
const POINT: &str = "class Point { pub x: i64; pub y: i64; } ";

// 38. a class-typed parameter and a class-typed return are eligible
#[test]
fn e38_class_param_and_return_eligible() {
    let src = format!("{POINT} fn f(p: Point) -> Point {{ return p; }}");
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// 39. `new` through the implicit memberwise constructor is eligible
#[test]
fn e39_new_memberwise_eligible() {
    let src = format!("{POINT} fn f() -> i64 {{ let p = new Point(1, 2); return p.x + p.y; }}");
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// 40. `new` through an explicit `init` constructor is eligible
#[test]
fn e40_new_explicit_init_eligible() {
    let src = "class Counter { pub n: i64; \
                   pub init(self, n: i64) { self.n = n; } } \
                   fn f() -> i64 { let c = new Counter(7); return c.n; }";
    assert!(eligible_lenient(src, "f", &["f"]));
}

// 41. an object literal is eligible: it lowers to the same field stores.
//
// The type checker rejects `C { f: v }` in source today (check_ops.rs:
// "named field syntax is part of the old construction form"), so this and
// the next perspective exercise the walker's handling directly from HIR —
// the eligibility predicate must stay consistent with the emitter for the
// node it can still be handed.
#[test]
fn e41_object_literal_eligible() {
    let src = format!("{POINT} fn f() -> i64 {{ let p = Point {{ x: 1, y: 2 }}; return p.y; }}");
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// 42. an object literal that omits a declared field is not claimed: the
// walker only emits a complete memberwise initialisation.
#[test]
fn e42_partial_object_literal_ineligible() {
    let src = format!("{POINT} fn f() -> i64 {{ let p = Point {{ x: 1 }}; return p.x; }}");
    assert!(!eligible_lenient(&src, "f", &["f"]));
}

// 43. reading a field is eligible
#[test]
fn e43_field_read_eligible() {
    let src = format!("{POINT} fn f(p: Point) -> i64 {{ return p.x; }}");
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// 44. assigning a field is eligible (a LIR instruction, not an expression)
#[test]
fn e44_field_assign_eligible() {
    let src = format!("{POINT} fn f() -> i64 {{ let p = new Point(1, 2); p.x = 9; return p.x; }}");
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// 45. a chained field read walks two statically non-optional objects
#[test]
fn e45_nested_field_read_eligible() {
    let src = "class Inner { pub v: i64; } class Outer { pub inner: Inner; } \
                   fn f(o: Outer) -> i64 { return o.inner.v; }";
    assert!(eligible_lenient(src, "f", &["f"]));
}

// 46. an instance method call is eligible: a direct call to `Class__method`
#[test]
fn e46_instance_method_call_eligible() {
    let src = "class Counter { pub n: i64; \
                   pub fn get(self) -> i64 { return self.n; } } \
                   fn f(c: Counter) -> i64 { return c.get(); }";
    assert!(eligible_lenient(src, "f", &["f"]));
}

// 47. a static method call is eligible (a null receiver is passed)
#[test]
fn e47_static_method_call_eligible() {
    let src = "class Counter { pub n: i64; \
                   pub static fn zero() -> i64 { return 0; } } \
                   fn f() -> i64 { return Counter::zero(); }";
    assert!(eligible_lenient(src, "f", &["f"]));
}

// 48. a GC-managed field type is eligible: the store goes through the
// object-field write path, not a plain store
#[test]
fn e48_string_field_eligible() {
    let src = "class Item { pub name: String; } \
                   fn f() -> String { let i = new Item(\"a\"); i.name = \"b\"; return i.name; }";
    assert!(eligible_lenient(src, "f", &["f"]));
}

// 49. an `Array<T>` field is eligible: the element type is checked too
#[test]
fn e49_array_field_eligible() {
    let src = "import std::collections::Array; class Bag { pub xs: Array<i64>; } \
                   fn f(b: Bag) -> i64 { return b.xs.len(); }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// 50. a class local that lives across a loop is eligible: its root slot is
// allocated once at entry, so the shadow stack does not grow per iteration
#[test]
fn e50_class_local_across_loop_eligible() {
    let src = format!(
        "{POINT} fn f() -> i64 {{ let p = new Point(0, 0); let mut i = 0; \
             while i < 3 {{ p.x = p.x + i; i = i + 1; }} return p.x; }}"
    );
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// 51. a subclass is IN the subset. Its layout
// is its base's fields followed by its own — the order
// `finalize_class_layouts` builds and the one the walker now models — and a
// method call on it is routed by `plan_virtual_call`, the one function that
// decides which implementation a call site runs.
#[test]
fn e51_subclass_eligible() {
    let src = "pub open class Animal { pub age: i64; } \
                   pub class Dog extends Animal { } \
                   fn f(d: Dog) -> i64 { return d.age; }";
    assert!(eligible_lenient(src, "f", &["f"]));
}

// 52. a BASE-typed slot is in too. A caller
// may hand it a subclass instance, but a subclass's layout EXTENDS its
// base's, so `a.age` sits at the same offset either way; anything virtual
// on `a` goes through the descriptor slot rather than being bound to
// `Animal__..` by name.
#[test]
fn e52_base_class_eligible() {
    let src = "pub open class Animal { pub age: i64; } \
                   pub class Dog extends Animal { } \
                   fn f(a: Animal) -> i64 { return a.age; }";
    assert!(eligible_lenient(src, "f", &["f"]));
}

// 53. an interface-typed field was rejected while the walker emitted no
// boxing; now it is a supported field type (the store into it
// boxes), so a class that has one is still SIMPLE.
#[test]
fn e53_interface_field_eligible() {
    let src = "interface Named { fn name(self) -> String; } \
                   class Holder { pub n: Named; } \
                   fn f(h: Holder) -> i64 { return 1; }";
    assert!(eligible_lenient(src, "f", &["f"]));
}

// 54. an enum-typed field was rejected while the
// walker had no enum layout; now that it does, a class holding one is SIMPLE
// and reading the field is eligible.
#[test]
fn e54_enum_field_eligible() {
    let src = "enum Color { Red, Green } \
                   class Holder { pub c: Color; } \
                   fn f(h: Holder) -> i64 { return 1; }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// 55. DISPATCHING through an interface parameter is rejected. Since
//  the parameter TYPE is fine (see j03); it is the virtual call
// through the box's vtable that the walker does not emit.
#[test]
fn e55_interface_param_ineligible() {
    let src = "interface Named { fn name(self) -> String; } \
                   fn f(n: Named) -> String { return n.name(); }";
    assert!(!eligible_lenient(src, "f", &["f"]));
}

// 56. a widening `let` annotation whose value is then DISPATCHED on is
// rejected. The widening itself is emittable now (see j01) —
// and it is exactly the case that makes `HirStmt::Let::ty` rather than
// `value.ty` the type the walker must trust — but `x.name()` is not.
#[test]
fn e56_widening_let_annotation_ineligible() {
    let src = "interface Named { fn name(self) -> String; } \
                   class Item implements Named { pub n: String; \
                   pub fn name(self) -> String { return self.n; } } \
                   fn f() -> String { let x: Named = new Item(\"a\"); return x.name(); }";
    assert!(!eligible_lenient(src, "f", &["f"]));
}

// 57. a method call passes the address of the caller's place for a
// by-reference parameter.
#[test]
fn e57_reference_param_method_eligible() {
    let src = "class Counter { pub n: i64; \
                   pub fn bump(self, v: &mut i64) { v = v + 1; } } \
                   fn f(c: Counter) -> i64 { let mut k = 1; c.bump(&k); return k; }";
    assert!(
        eligible_lenient(src, "f", &["f"]),
        "{:?}",
        reason_of(src, "f", &["f"])
    );
}

// 58. calling a method ON the interface a method returned is rejected. The
// interface-returning method itself is fine now (see j05);
// the second `.name()` hop is the virtual dispatch that is not.
#[test]
fn e58_interface_returning_method_ineligible() {
    let src = "interface Named { fn name(self) -> String; } \
                   class Item implements Named { pub n: String; \
                   pub fn name(self) -> String { return self.n; } \
                   pub fn as_named(self) -> Named { return self; } } \
                   fn f(i: Item) -> String { return i.as_named().name(); }";
    assert!(!eligible_lenient(src, "f", &["f"]));
}

// 59. a self-referential field type must not send the support check into
// infinite recursion; a linked node is a perfectly ordinary GC handle
#[test]
fn e59_self_referential_class_eligible() {
    let src = "class Node { pub v: i64; pub next: Node; } \
                   fn f(n: Node) -> i64 { return n.v; }";
    assert!(eligible_lenient(src, "f", &["f"]));
}

// 60. an array of simple class objects, indexed and field-read, is eligible
#[test]
fn e60_class_array_field_read_eligible() {
    let src = format!(
        "import std::collections::Array; {POINT} fn f(ps: Array<Point>) -> i64 {{ return ps[0].x; }}"
    );
    assert!(eligible_checked(&src, "f", &["f"]));
}

// 61. Option-wrapped class types are in the subset as of:
// `Node?` is canonical `Option<Node>`, an enum instance whose payload is a
// simple class. The self-reference through the option (`next: Node?`) must
// still terminate the type walk, exactly as the direct one in perspective
// 59 does.
#[test]
fn e61_optional_class_eligible() {
    let field = "class Node { pub v: i64; pub next: Node?; } \
                     fn f(n: Node) -> i64 { return n.v; }";
    assert!(eligible_lenient(field, "f", &["f"]));

    let param = format!("{POINT} fn f(p: Point?) -> i64 {{ return 1; }}");
    assert!(eligible_lenient(&param, "f", &["f"]));

    // A class in an inheritance hierarchy is in the subset as of
    //, so an option over one is in too: the payload is one
    // object pointer whichever concrete class it holds.
    let inherited = "open class Base { pub v: i64; } class Sub extends Base {} \
                         fn f(b: Base?) -> i64 { return 1; }";
    assert!(eligible_lenient(inherited, "f", &["f"]));

    // A generic interface remains a supported pointer payload in Option.
    let generic_interface = "interface Boxed<T> { fn get(self) -> T; } \
                           fn f(b: Boxed<String>?) -> i64 { return 1; }";
    assert!(eligible_lenient(generic_interface, "f", &["f"]));
}
// 62. a class reached through a DIRECT TYPE IMPORT is the same class as its
// module-qualified self, so every ancestry question about it must be asked
// in `type_id` space. `import zoo::Animal;` registers the class a second
// time under `Animal`, sharing the canonical `type_id`, while `class_base`
// keeps canonical names on both sides (`zoo::Dog` -> `zoo::Animal`) — so
// the name `Animal` never appears in the base chain at all. Comparing names
// would call the two unrelated and reject a valid store.
#[test]
fn e62_imported_base_class_alias_widens_by_type_id() {
    let src = "class Animal { pub value: i64; } fn f(a: Animal) -> i64 { return a.value; }";
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "{errs:?}");
    let (hir, diags) = crate::ir::lower::lower_program(&program);
    assert!(diags.is_empty(), "{diags:?}");
    let p = crate::ir::lowered::lower_program(&hir);
    let f = p
        .functions
        .iter()
        .find(|f| f.name.to_string() == "f")
        .unwrap();

    let mut tables = TestTables::build(&program, &["f"], &[]);
    assert!(tables.with_ctx(|ctx| lir_supported_function(f, ctx)));

    // Now model the import: `Animal` is an alias of `zoo::Animal`, which
    // `zoo::Dog` extends. Only the canonical names appear in `class_base`.
    let animal_id = tables.class_type_ids["Animal"];
    let animal_layout = tables.class_layouts["Animal"].clone();
    tables
        .class_type_ids
        .insert("zoo::Animal".to_string(), animal_id);
    tables
        .class_type_ids
        .insert("zoo::Dog".to_string(), animal_id + 100);
    tables
        .class_layouts
        .insert("zoo::Animal".to_string(), animal_layout.clone());
    tables
        .class_layouts
        .insert("zoo::Dog".to_string(), animal_layout);
    tables
        .class_base
        .insert("zoo::Dog".to_string(), "zoo::Animal".to_string().into());

    let dog = Type::Named("zoo::Dog".to_string().into());
    let animal = Type::Named("Animal".to_string().into());
    assert!(
        tables.with_ctx(|ctx| ctx.class_widening(&animal, &dog)),
        "a subclass must fit its base's slot through the alias spelling"
    );
    //... and the relation stays directional: the base does not fit a
    // subclass slot, whichever name it is reached by.
    assert!(!tables.with_ctx(|ctx| ctx.class_widening(&dog, &animal)));
    assert!(!tables.with_ctx(|ctx| {
        ctx.class_widening(&dog, &Type::Named("zoo::Animal".to_string().into()))
    }));
}

// 63. an object literal that names the same field twice is rejected even
// though the COUNT matches the layout: the emitter would store into that
// field twice and leave the other one at its zero value.
#[test]
fn e63_object_literal_duplicate_field_ineligible() {
    let src = format!("{POINT} fn f() -> i64 {{ let p = Point {{ x: 1, x: 2 }}; return p.x; }}");
    assert!(!eligible_lenient(&src, "f", &["f"]));
}

// 64. an array handle's element contract is part of its representation:
// scalar/reference classification and class/interface boxing differ even
// though every source-level Array value is carried as one pointer.
#[test]
fn e64_array_representation_requires_exact_element_type() {
    let array = |element| Type::Array(Box::new(element));
    assert!(assignable_repr(&array(Type::I64), &array(Type::I64)));
    assert!(!assignable_repr(&array(Type::I64), &array(Type::String)));
    assert!(!assignable_repr(
        &array(Type::Named("Point".to_string().into())),
        &array(Type::Named("Other".to_string().into()))
    ));
    assert!(!assignable_repr(&array(Type::String), &array(Type::Void)));
}

// ---------------------------------------------------------------------
// — class → interface boxing coercion in the LIR walker.
//
// An interface value is a 16-byte `[object | vtable]` GC box, so putting a
// class instance in an interface-typed slot is a conversion, not a
// reinterpretation. The walker now emits it at every STORE position, and
// only there. Interface dispatch is covered separately;
// interfaces do not expose a concrete field layout.
//
// Perspectives j01-j21 below are the eligibility half; j22-j36 live in
// `tests/integration/codegen.rs` as differential and
// `WILLOW_GC_STRESS=alloc` runs, because they are about the emitted code
// and its GC rooting.
//
// j01. `let x: Iface = new C();` — widening let init
// j02. `x = new C();` — widening assignment to an interface local
// j03. an interface-typed parameter, passed along without dispatching
// j04. a class argument boxed into an interface parameter
// j05. `return new C();` from an interface-returning function
// j06. `h.n = new C();` — widening store into an interface-typed field
// j07. `let xs: Array<Iface> = [new C()]` is REJECTED — an array literal is
//      typed by its elements and there is no per-handle conversion
// j08. `xs.push(new C())` on an `Array<Iface>`
// j09. `new Holder(new C())` — memberwise constructor field boxing
// j10. an explicit `init` with an interface parameter
// j11. a static method with an interface parameter
// j12. reading an interface-typed field is eligible (a plain load)
// j13. `xs[0] = new C();` — index-assign into an `Array<Iface>`
// j14. an interface value stored into the SAME interface needs no box
// j15. a class with no vtable for that interface is rejected — the
//      emitter's fallback is to pass the object through UNBOXED
// j16. a class taking part in inheritance cannot be boxed by the walker
// j17. interface → a DIFFERENT interface is rejected (no re-boxing)
// j18. a generic interface instantiation (`Box<String>`) is rejected
// j19. an optional interface (`Iface?`) is rejected
// j20. a ternary whose arms are classes but whose type is the interface is
//      rejected: both arms feed one variable and neither gets boxed
// j21. `Array<Iface>.toString()` is rejected — no element kind
// ---------------------------------------------------------------------

/// An interface, a simple class implementing it, and a holder class with an
/// interface-typed field. Reused by the perspectives below.
const NAMED: &str = "interface Named { fn name(self) -> String; } \
                         class Item implements Named { pub n: String; \
                         pub fn name(self) -> String { return self.n; } } \
                         class Holder { pub n: Named; } ";

// j01. a widening `let` initialiser is eligible on its own
#[test]
fn j01_widening_let_eligible() {
    let src =
        format!("{NAMED} fn f() -> i64 {{ let x: Named = new Item(\"a\"); let y = 1; return y; }}");
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// j02. a widening assignment to an interface-typed local is eligible
#[test]
fn j02_widening_assign_eligible() {
    let src = format!(
        "{NAMED} fn f(seed: Named) -> i64 {{ let mut x: Named = seed; \
             x = new Item(\"a\"); return 1; }}"
    );
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// j03. an interface-typed parameter is fine as long as nothing dispatches
// on it: it is a GC handle like any other (contrast e55).
#[test]
fn j03_interface_param_passthrough_eligible() {
    let src = format!("{NAMED} fn f(n: Named) -> Named {{ return n; }}");
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// j04. a class argument is boxed into an interface parameter at the call
#[test]
fn j04_boxed_call_argument_eligible() {
    let src = format!(
        "{NAMED} fn g(n: Named) -> i64 {{ return 1; }} \
             fn f() -> i64 {{ return g(new Item(\"a\")); }}"
    );
    assert!(eligible_lenient(&src, "f", &["f", "g"]));
}

// j05. `return new Item(..)` from an interface-returning function boxes
#[test]
fn j05_boxed_return_eligible() {
    let src = format!("{NAMED} fn f() -> Named {{ return new Item(\"a\"); }}");
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// j06. a widening store into an interface-typed field boxes
#[test]
fn j06_boxed_field_assign_eligible() {
    let src = format!("{NAMED} fn f(h: Holder) -> i64 {{ h.n = new Item(\"a\"); return 1; }}");
    assert!(eligible_lenient(&src, "f", &["f"]));
}

#[test]
fn contextual_arrays_generated_stores_scale_linearly() {
    use crate::ir::lowered::{LirInst, LirRvalue};
    for n in [8, 16, 32, 64] {
        let elements = vec!["new Item(\"a\")"; n].join(",");
        let source = format!(
            "import std::collections::Array; {NAMED} fn f() -> Array<Named> {{ return [{elements}]; }}"
        );
        let (f, tables) = lir_fn_and_tables(&source, "f", &["f"]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
        let stores = f
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter(|inst| {
                matches!(
                    inst,
                    LirInst::Compute {
                        value: LirRvalue::ArrayStore { .. },
                        ..
                    }
                )
            })
            .count();
        assert_eq!(stores, n);
        eprintln!("contextual array generated stores: n={n}: {stores}");
    }
}

// j07. an ANNOTATED array literal takes the
// annotation's element type — `lower.rs` retypes it, as the checker's
// `check_array_literal_expecting` already did — so
// `let xs: Array<Named> = [new Item("a")]` reaches the walker as an
// `Array<Named>` whose ELEMENTS each need boxing, not as an `Array<Item>`
// handle needing a conversion that does not exist. The literal emitter
// stores every element through `coerce_to_target`, so each one is boxed
// individually and the handle itself is already the right thing.
#[test]
fn j07_widening_array_literal_boxes_each_element() {
    let src = format!(
        "import std::collections::Array; {NAMED} \
             fn f() -> i64 {{ let xs: Array<Named> = [new Item(\"a\")]; return xs.len(); }}"
    );
    assert!(eligible_checked(&src, "f", &["f"]));

    // An array literal whose elements ALREADY match the slot is fine.
    let exact = format!(
        "import std::collections::Array; {NAMED} \
             fn f(n: Named) -> i64 {{ let xs: Array<Named> = [n]; return xs.len(); }}"
    );
    assert!(eligible_checked(&exact, "f", &["f"]));

    // The per-element boxing is a real requirement, not an assumption: with
    // the vtable table emptied there is no box to build, and the same
    // literal fails validation rather than storing a bare `Item` into an
    // interface-typed element slot.
    let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
    assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    tables.vtables.clear();
    assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));

    // Element types are still compared exactly where no coercion runs: it
    // is why [`assignable_repr`] looks INSIDE an array handle rather than
    // calling any two of them interchangeable.
    assert!(!assignable_repr(
        &Type::Array(Box::new(Type::Named("Named".to_string().into()))),
        &Type::Array(Box::new(Type::Named("Item".to_string().into())))
    ));
}

// j08. `push` onto an `Array<Iface>` boxes its argument. The array comes in
// as a parameter because an empty literal never reaches the walker at all
// (see e32), which would mask the property under test.
#[test]
fn j08_boxed_array_push_eligible() {
    let src = format!(
        "import std::collections::Array; {NAMED} fn f(xs: Array<Named>) -> i64 {{ \
             xs.push(new Item(\"a\")); return xs.len(); }}"
    );
    assert!(eligible_checked(&src, "f", &["f"]));
}

// j09. the implicit memberwise constructor boxes into an interface field
#[test]
fn j09_boxed_memberwise_new_eligible() {
    let src = format!("{NAMED} fn f() -> i64 {{ let h = new Holder(new Item(\"a\")); return 1; }}");
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// j10. an explicit `init` taking an interface parameter boxes at the call
#[test]
fn j10_boxed_explicit_init_eligible() {
    let src = format!(
        "{NAMED} class Wrap {{ pub n: Named; \
             pub init(self, n: Named) {{ self.n = n; }} }} \
             fn f() -> i64 {{ let w = new Wrap(new Item(\"a\")); return 1; }}"
    );
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// j11. a static method taking an interface parameter boxes at the call
#[test]
fn j11_boxed_static_call_argument_eligible() {
    let src = format!(
        "{NAMED} class Util {{ pub static fn count(n: Named) -> i64 {{ return 1; }} }} \
             fn f() -> i64 {{ return Util::count(new Item(\"a\")); }}"
    );
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// j12. READING an interface-typed field is a plain load, no coercion
#[test]
fn j12_interface_field_read_eligible() {
    let src = format!("{NAMED} fn f(h: Holder) -> Named {{ return h.n; }}");
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// j13. index-assignment into an `Array<Iface>` boxes the element
#[test]
fn j13_boxed_index_assign_eligible() {
    let src = format!(
        "import std::collections::Array; {NAMED} fn f(xs: Array<Named>) -> i64 {{ xs[0] = new Item(\"a\"); return xs.len(); }}"
    );
    assert!(eligible_checked(&src, "f", &["f"]));
}

// j14. an interface value moved into the SAME interface is already boxed:
// `storable` must accept it without asking for a second box.
#[test]
fn j14_same_interface_store_needs_no_box() {
    let src = format!("{NAMED} fn f(n: Named) -> i64 {{ let x: Named = n; return 1; }}");
    assert!(eligible_lenient(&src, "f", &["f"]));
}

// j15. THE safety property: a class with no registered vtable for the
// target interface must not be admitted. `coerce_to_target` answers a
// missing vtable by returning the object UNBOXED, which would put a raw
// class pointer in an interface slot and crash the first dispatch on it.
// Source cannot express this (the checker demands `implements`), so drive
// the predicate directly with the vtable table emptied.
#[test]
fn j15_boxing_without_a_vtable_is_rejected() {
    let src = format!("{NAMED} fn f() -> Named {{ return new Item(\"a\"); }}");
    let tokens = Lexer::new(&src).tokenize().expect("lex");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "{errs:?}");
    let (hir, diags) = crate::ir::lower::lower_program(&program);
    assert!(diags.is_empty(), "{diags:?}");
    let p = crate::ir::lowered::lower_program(&hir);
    let f = p
        .functions
        .iter()
        .find(|f| f.name.to_string() == "f")
        .unwrap();

    let mut tables = TestTables::build(&program, &["f"], &[]);
    assert!(tables.with_ctx(|ctx| lir_supported_function(f, ctx)));
    tables.vtables.clear();
    assert!(!tables.with_ctx(|ctx| lir_supported_function(f, ctx)));
}

// j16. a class that takes part in inheritance
// is an ordinary box SOURCE: the box carries the object pointer and the
// interface vtable of the CONCRETE class, which is emitted per class
// regardless of what that class extends.
#[test]
fn j16_boxing_an_inheriting_class_is_eligible() {
    let src = "interface Named { fn name(self) -> String; } \
                   pub open class Animal { pub age: i64; } \
                   pub class Dog extends Animal implements Named { \
                   pub fn name(self) -> String { return \"dog\"; } } \
                   fn f() -> Named { return new Dog(1); }";
    assert!(eligible_lenient(src, "f", &["f"]));
}

// j17. interface → SUPER-interface is the
// coercion `coerce_to_target` performs by doing NOTHING: the same box is
// reused. That is sound exactly when the target's vtable slots are a PREFIX
// of the source's, which is what the walker tests — not "is a super",
// because composition puts a second super's methods AFTER the first's.
//
// Checked, not lenient: without desugar the interface method order is
// uncomposed, so a lenient run would answer from a `B` that does not yet
// carry `A`'s methods at all — passing for a reason unrelated to the rule.
#[test]
fn j17_interface_to_super_interface_eligible() {
    let single = "interface A { fn a(self) -> i64; } \
                      interface B extends A { fn b(self) -> i64; } \
                      fn f(x: B) -> A { return x; }";
    assert!(eligible_checked(single, "f", &["f"]));

    // The prefix runs one way only: an `A` box has no slot for `b`.
    let narrowing = "interface A { fn a(self) -> i64; } \
                         interface B extends A { fn b(self) -> i64; } \
                         fn f(x: B) -> A { return x; } \
                         fn g(x: A) -> i64 { return x.a(); }";
    assert!(eligible_checked(narrowing, "g", &["f", "g"]));
}

// j17b. with TWO supers the second one's table starts at a non-zero offset.
// The walker admits it now that `coerce_to_target` allocates a rewidened
// box pointing at that embedded region.
#[test]
fn j17b_interface_to_second_super_eligible() {
    let decls = "interface A { fn a(self) -> i64; } \
                     interface B { fn b(self) -> i64; } \
                     interface C extends A, B { fn c(self) -> i64; } ";
    let second = format!("{decls} fn f(x: C) -> B {{ return x; }}");
    assert!(eligible_checked(&second, "f", &["f"]));

    //... while widening the same `C` to its FIRST super stays in, so this
    // does not pass by rejecting multi-super interfaces wholesale.
    let first = format!("{decls} fn f(x: C) -> A {{ return x; }}");
    assert!(eligible_checked(&first, "f", &["f"]));
}

// j18. a class can be boxed into a concrete generic interface target.
#[test]
fn j18_generic_interface_target_eligible() {
    let src = "interface Boxed<T> { fn get(self) -> T; } \
                   class SBox implements Boxed<String> { pub v: String; \
                   pub fn get(self) -> String { return self.v; } } \
                   fn f() -> Boxed<String> { return new SBox(\"a\"); }";
    assert!(eligible_lenient(src, "f", &["f"]));
}

// j19. an optional interface is rejected: Option is outside the walker's
// supported representation set.
#[test]
fn j19_optional_interface_rejected() {
    let src = format!(
        "{NAMED} enum Option<T> {{ Some(T), None, }} \
             fn f() -> i64 {{ let x: Option<Named> = Option::None; return 1; }}"
    );
    assert!(!eligible_lenient(&src, "f", &["f"]));
}

// j20. both ternary arms define ONE Cranelift variable and the walker
// inserts no conversion between them, so a ternary that widens to the
// interface must fail validation rather than store two raw class pointers.
#[test]
fn j20_widening_ternary_rejected() {
    let src = format!(
        "{NAMED} class Other implements Named {{ pub m: String; \
             pub fn name(self) -> String {{ return self.m; }} }} \
             fn f(c: bool) -> Named {{ return c ? new Item(\"a\") : new Other(\"b\"); }}"
    );
    assert!(!eligible_lenient(&src, "f", &["f"]));
}

// j21. `toString()` on an `Array<Iface>` has no runtime element kind
#[test]
fn j21_interface_array_to_string_rejected() {
    let src = format!("{NAMED} fn f(xs: Array<Named>) -> String {{ return xs.toString(); }}");
    assert!(!eligible_lenient(&src, "f", &["f"]));
}

// ---------------------------------------------------------------------
// — interface DISPATCH eligibility (k01..k27).
//
// The receiver is a box, so the walker never needs the concrete class; what
// it does need is the vtable SLOT, and the only thing standing between a
// wrong slot and silent miscompilation is that eligibility resolves the
// method exactly the way the emitter will. Perspectives below therefore
// split into: shapes the walker must CLAIM (k01..k13, k18, k23), shapes it
// must REFUSE (k14..k17, k19), drift between the interface tables and the
// call site, which source cannot express and which is driven through the
// predicate directly (k20..k22), and parameter MODES, where the declared
// `&`/`&mut` is part of the ABI the walker cannot emit (k24..k27).
// ---------------------------------------------------------------------

/// A three-method interface: `Named` has a single method, so it cannot tell
/// slot 0 from "the only slot there is". `describe` sits at slot 1 and
/// takes arguments; `tally` at slot 2 returns void.
const MULTI: &str = "interface Shape { \
                         fn area(self) -> i64; \
                         fn describe(self, prefix: String, n: i64) -> String; \
                         fn stamp(self); } \
                         class Sq implements Shape { pub side: i64; \
                         pub fn area(self) -> i64 { return self.side * self.side; } \
                         pub fn describe(self, prefix: String, n: i64) -> String { \
                         return prefix + n.toString(); } \
                         pub fn stamp(self) { println(self.side); } } ";

/// Two interfaces, because an interface cannot name ITSELF as a return type
/// (E0350): `Chain::next` hands back a `Leaf` box, which is then the
/// receiver of a second dispatch.
const CHAIN: &str = "interface Leaf { fn v(self) -> i64; } \
                         interface Chain { fn next(self) -> Leaf; } \
                         class L implements Leaf { pub k: i64; \
                         pub fn v(self) -> i64 { return self.k; } } \
                         class C implements Chain { pub k: i64; \
                         pub fn next(self) -> Leaf { return new L(self.k); } } ";

// k01. the base case: dispatch on an interface-typed PARAMETER. Contrast
// j03, which only passed the same value through without calling on it.
#[test]
fn k01_dispatch_on_interface_param_eligible() {
    let src = format!("{NAMED} fn f(n: Named) -> String {{ return n.name(); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// k02. dispatch on an interface LOCAL that the walker itself boxed: the box
// it writes must be the box it then reads `[object | vtable]` out of.
#[test]
fn k02_dispatch_on_boxed_local_eligible() {
    let src =
        format!("{NAMED} fn f() -> String {{ let x: Named = new Item(\"a\"); return x.name(); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// k03. the receiver is an interface-typed FIELD read, not a variable
#[test]
fn k03_dispatch_on_field_receiver_eligible() {
    let src = format!("{NAMED} fn f(h: Holder) -> String {{ return h.n.name(); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// k04. the receiver is a temporary — the result of a call. Nothing else
// holds it, so the emitter's object root is the only thing keeping it alive
// across argument evaluation.
#[test]
fn k04_dispatch_on_call_result_receiver_eligible() {
    let src = format!(
        "{NAMED} fn make() -> Named {{ return new Item(\"a\"); }} \
             fn f() -> String {{ return make().name(); }}"
    );
    assert!(eligible_checked(&src, "f", &["f", "make"]));
}

// k05. the receiver is an element of an `Array<Iface>`. `Array` is a
// collection type, so the checker demands the import even though these
// tests never resolve a module.
#[test]
fn k05_dispatch_on_array_element_receiver_eligible() {
    let src = format!(
        "import std::collections::Array; {NAMED} \
             fn f(xs: Array<Named>) -> String {{ return xs[0].name(); }}"
    );
    assert!(eligible_checked(&src, "f", &["f"]));
}

// k06. a method at a NON-ZERO vtable slot, with arguments. `describe` is
// slot 1 of three; picking slot 0 would call `area` with the wrong
// signature, which is the failure this whole subset guards against.
#[test]
fn k06_dispatch_on_later_slot_with_args_eligible() {
    let src = format!("{MULTI} fn f(s: Shape) -> String {{ return s.describe(\"n=\", 3); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// k07. a VOID-returning method in statement position: the call produces no
// Cranelift result and the walker must not read one.
#[test]
fn k07_dispatch_void_method_statement_eligible() {
    let src = format!("{MULTI} fn f(s: Shape) -> i64 {{ s.stamp(); return 1; }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// k08. the argument to an interface method is itself widened to an
// interface — boxing and dispatch composed at one call
#[test]
fn k08_dispatch_with_boxed_argument_eligible() {
    let src = format!(
        "{NAMED} interface Visitor {{ fn visit(self, n: Named) -> i64; }} \
             class V implements Visitor {{ pub k: i64; \
             pub fn visit(self, n: Named) -> i64 {{ return self.k; }} }} \
             fn f(v: Visitor) -> i64 {{ return v.visit(new Item(\"a\")); }}"
    );
    assert!(eligible_checked(&src, "f", &["f"]));
}

// k09. an interface method returning ANOTHER interface value, then
// dispatching on that result: the returned box is used as a receiver
// without ever being stored.
#[test]
fn k09_chained_interface_dispatch_eligible() {
    let src = format!("{CHAIN} fn f(c: Chain) -> i64 {{ return c.next().v(); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// k10. a `Self`-returning method: the callee hands back a BARE object of
// the receiver's class, which the emitter re-boxes with the receiver's own
// vtable, so the result is the RECEIVER'S interface and nothing else.
//
// Source cannot reach this arm today: the checker resolves `Self` only on a
// GENERIC interface (E0350 otherwise), and a generic receiver is a
// `Type::Generic` the subset already refuses (k14). The arm is therefore a
// guard for the day generic interfaces are admitted — and an interface
// cannot name itself as a return type either, so the call site is built
// directly rather than parsed.
#[test]
fn k10_self_returning_method_matches_receiver_interface() {
    let src = format!("{MULTI} fn f(s: Shape) -> i64 {{ return s.area(); }}");
    let (_, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
    tables.iface_methods.get_mut("Shape").unwrap().push((
        "itself".to_string(),
        Vec::new(),
        Vec::new(),
        Type::Named("Self".to_string().into()),
    ));
    let shape = Type::Named("Shape".to_string().into());
    let call = |ty: Type| HirExpr {
        kind: HirExprKind::MethodCall {
            object: Box::new(HirExpr {
                kind: HirExprKind::Var("s".to_string()),
                ty: shape.clone(),
                span: crate::diagnostics::Span::dummy(),
            }),
            method: "itself".to_string(),
            args: Vec::new(),
            super_class: None,
        },
        ty,
        span: crate::diagnostics::Span::dummy(),
    };
    let names: HashMap<&str, Cow<'_, Type>> = HashMap::from([("s", Cow::Borrowed(&shape))]);
    let as_receiver_iface = call(shape.clone());
    let as_other_type = call(Type::String);
    tables.with_ctx(|ctx| {
        assert!(supported_expr(&as_receiver_iface, ctx, &names));
        // Anything but the receiver's own interface: the re-box produces a
        // `Shape` box, so a `String` consumer would get a pointer.
        assert!(!supported_expr(&as_other_type, ctx, &names));
    });
}

// k11. the dispatch result feeds a further store that BOXES: interface in,
// interface out, one more box on the way into the slot
#[test]
fn k11_dispatch_result_stored_in_interface_local_eligible() {
    let src = format!("{CHAIN} fn f(c: Chain) -> i64 {{ let n: Leaf = c.next(); return n.v(); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// k12. dispatch inside a loop: the receiver root and the call frame must
// balance per iteration, not accumulate (the reason LIR roots are entry
// slots rather than per-`let` pushes).
#[test]
fn k12_dispatch_in_loop_eligible() {
    let src = format!(
        "{MULTI} fn f(s: Shape) -> i64 {{ let mut i = 0; let mut t = 0; \
             while i < 3 {{ t = t + s.area(); i = i + 1; }} return t; }}"
    );
    assert!(eligible_checked(&src, "f", &["f"]));
}

// k13. dispatch as an ARGUMENT to another call, and in a condition — the
// walker must handle it anywhere an expression is legal, not only in
// `return`/`let` position.
#[test]
fn k13_dispatch_nested_in_call_and_condition_eligible() {
    let src = format!(
        "{MULTI} fn g(v: i64) -> i64 {{ return v; }} \
             fn f(s: Shape) -> i64 {{ if s.area() > 0 {{ return g(s.area()); }} return 0; }}"
    );
    assert!(eligible_checked(&src, "f", &["f", "g"]));
}

// k14. a GENERIC interface dispatches through the bare-name vtable with a
// signature instantiated from the receiver's type arguments.
#[test]
fn k14_generic_interface_receiver_eligible() {
    let src = "interface Boxed<T> { fn get(self) -> T; } \
                   class SBox implements Boxed<String> { pub v: String; \
                   pub fn get(self) -> String { return self.v; } } \
                   fn f(b: Boxed<String>) -> String { return b.get(); }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// k15. an OPTIONAL interface receiver: the payload is unwrapped by the
// match and dispatched through the same vtable slot a bare interface
// parameter would use. Both arms `return`, so the match itself is typed
// `!` — admitted since, which is what brought this shape
// (and `example/option_interface_context.wi`) onto the walker.
#[test]
fn k15_optional_interface_receiver_eligible() {
    let src = format!(
        "{NAMED} enum Option<T> {{ Some(T), None, }} \
             fn f(n: Option<Named>) -> String {{ match n {{ \
             Some(value) => return value.name(), None => return \"x\", }} }}"
    );
    assert!(eligible_checked(&src, "f", &["f"]));
}

// k16. a CLASS receiver is unaffected by this arm: it still resolves the
// concrete `Class__method` symbol, so the interface arm must be checked
// first without swallowing the class case.
#[test]
fn k16_class_receiver_still_takes_the_class_path() {
    let src = format!("{NAMED} fn f(i: Item) -> String {{ return i.name(); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// k17. an interface whose method is not registered in the tables at all
// (the whole interface unknown to `iface_method`) must be refused rather
// than fall through to the class arm and mangle a `Named__name` symbol.
#[test]
fn k17_unregistered_interface_method_rejected() {
    let src = format!("{NAMED} fn f(n: Named) -> String {{ return n.name(); }}");
    let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
    assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    tables.iface_methods.clear();
    assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
}

// k18. a DEFAULT interface method occupies a vtable slot
// like any other: the implementing class inherits the body, so dispatch
// through the box must find it without the class declaring anything.
#[test]
fn k18_default_interface_method_eligible() {
    let src = "interface Greeter { fn name(self) -> String; \
                   fn greet(self) -> String { return \"hi \" + self.name(); } } \
                   class Dog implements Greeter { pub k: String; \
                   pub fn name(self) -> String { return self.k; } } \
                   fn f(g: Greeter) -> String { return g.greet(); }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// k19. an interface method taking an argument the walker cannot store —
// here a class argument with no vtable for the interface parameter — is
// refused by the same `storable` gate that guards every other store site.
#[test]
fn k19_unboxable_argument_rejected() {
    let src = format!(
        "{NAMED} interface Visitor {{ fn visit(self, n: Named) -> i64; }} \
             class V implements Visitor {{ pub k: i64; \
             pub fn visit(self, n: Named) -> i64 {{ return self.k; }} }} \
             fn f(v: Visitor) -> i64 {{ return v.visit(new Item(\"a\")); }}"
    );
    let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
    assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    tables.vtables.clear();
    assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
}

// k20. TABLE DRIFT, arity: if the interface's recorded signature and the
// call site disagree on argument count, the indirect call would be built
// from the wrong signature. Source cannot express this (the checker rejects
// it first), so drive the predicate with a doctored table.
#[test]
fn k20_arity_mismatch_rejected() {
    let src = format!("{MULTI} fn f(s: Shape) -> i64 {{ return s.area(); }}");
    let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
    assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    let methods = tables.iface_methods.get_mut("Shape").unwrap();
    methods[0].1.push(Type::I64);
    assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
}

// k21. TABLE DRIFT, return type: the call site's type and the declared
// return must be representation-compatible, or the walker would hand a
// caller a value of the wrong Cranelift type.
#[test]
fn k21_return_type_mismatch_rejected() {
    let src = format!("{MULTI} fn f(s: Shape) -> i64 {{ return s.area(); }}");
    let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
    let methods = tables.iface_methods.get_mut("Shape").unwrap();
    methods[0].3 = Type::String;
    assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
}

// k22. TABLE DRIFT, unsupported parameter type: a method whose signature
// mentions a type outside the subset must be refused even though the call
// site itself looks fine.
#[test]
fn k22_unsupported_parameter_type_rejected() {
    // Arity is left alone so the parameter TYPE is the only thing that can
    // decide the outcome.
    let src =
        format!("{MULTI} fn f(s: Shape, k: i64) -> String {{ return s.describe(\"n=\", k); }}");
    let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
    assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    let methods = tables.iface_methods.get_mut("Shape").unwrap();
    methods[1].1[1] = Type::Generic("Map".to_string().into(), vec![Type::String, Type::I64]);
    assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
}

// k23. an interface that INHERITS a method through `extends` dispatches on
// the COMPOSED slot list: desugaring folds the base interface's methods
// into the child's declaration, and the vtable is laid out from that. A
// table missing the inherited method must refuse the call rather than fall
// through to some other slot.
#[test]
fn k23_inherited_interface_method_dispatches_on_composed_slots() {
    let src = "interface Base { fn base(self) -> i64; } \
                   interface Ext extends Base { fn ext(self) -> i64; } \
                   class Impl implements Ext { pub k: i64; \
                   pub fn base(self) -> i64 { return self.k; } \
                   pub fn ext(self) -> i64 { return self.k + 1; } } \
                   fn f(e: Ext) -> i64 { return e.base() + e.ext(); }";
    let (f, mut tables) = lir_fn_and_tables(src, "f", &["f"]);
    assert!(
        tables.iface_methods["Ext"]
            .iter()
            .any(|(n, _, _, _)| n == "base"),
        "desugaring must compose the inherited method into `Ext`"
    );
    assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));

    tables
        .iface_methods
        .get_mut("Ext")
        .unwrap()
        .retain(|(n, _, _, _)| n != "base");
    assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
}

/// An interface whose slots differ ONLY in how their parameter is passed.
/// Every method takes one `i64`, so a walker that looks at types alone
/// cannot tell them apart — and the concrete methods behind two of these
/// slots receive a POINTER.
const MODES: &str = "interface Mode { \
                         fn by_value(self, v: i64) -> i64; \
                         fn by_mut(self, v: &mut i64); \
                         fn by_ref(self, v: & i64) -> i64; } \
                         class Impl implements Mode { pub k: i64; \
                         pub fn by_value(self, v: i64) -> i64 { return v + self.k; } \
                         pub fn by_mut(self, v: &mut i64) { v = v + self.k; } \
                         pub fn by_ref(self, v: & i64) -> i64 { return v + self.k; } } ";

/// Lower `src` the checked way and report the diagnostics instead of
/// asserting there are none: a reference ARGUMENT stops at HIR lowering,
/// which is a fact about the subset worth pinning rather than working
/// around.
fn checked_lowering_diags(src: &str) -> Vec<crate::diagnostics::Diagnostic> {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let (mut program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "{errs:?}");
    crate::desugar::DesugarPass::run(&mut program, &mut []);
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    let errors: Vec<_> = checker
        .errors
        .iter()
        .filter(|d| d.severity == crate::diagnostics::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    let tables = crate::ir::lower::SemanticFacts::from_checker(&checker);
    crate::ir::lower::lower_program_with(&program, &tables).1
}

// k24. Reference arguments survive lowering as explicit places, allowing
// the walker to preserve address semantics through virtual dispatch.
#[test]
fn k24_reference_argument_reaches_the_lir() {
    let src = "fn bump(x: &mut i64) { x = x + 1; } \
                   fn f() -> i64 { let mut x = 1; bump(&x); return x; }";
    let diags = checked_lowering_diags(src);
    assert!(diags.is_empty(), "reference lowering failed: {diags:?}");
    assert!(
        eligible_lenient(src, "f", &["bump", "f"]),
        "{:?}",
        reason_of(src, "f", &["bump", "f"])
    );
}

// k25. TABLE DRIFT, `&mut` parameter: a by-value argument must not be sent
// to a slot whose ABI now expects a pointer.
#[test]
fn k25_mut_reference_parameter_mode_rejected() {
    let src = format!("{MODES} fn f(m: Mode) -> i64 {{ return m.by_value(2); }}");
    let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
    assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));

    set_by_value_mode(
        &mut tables,
        ParamMode::Reference {
            mutable: true,
            ampersand_span: crate::diagnostics::Span::dummy(),
            mut_span: Some(crate::diagnostics::Span::dummy()),
        },
    );
    assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
}

// k26. the same for a SHARED `&` parameter: the ABI is a pointer whether or
// not the callee may write through it.
#[test]
fn k26_shared_reference_parameter_mode_rejected() {
    let src = format!("{MODES} fn f(m: Mode) -> i64 {{ return m.by_value(2); }}");
    let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
    set_by_value_mode(
        &mut tables,
        ParamMode::Reference {
            mutable: false,
            ampersand_span: crate::diagnostics::Span::dummy(),
            mut_span: None,
        },
    );
    assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
}

// k27. the control: restoring the by-value mode — same interface, same
// parameter TYPE, same call site — is claimed again, so k25/k26 cannot
// pass for some unrelated reason.
#[test]
fn k27_value_parameter_mode_still_eligible() {
    let src = format!("{MODES} fn f(m: Mode) -> i64 {{ return m.by_value(2); }}");
    let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
    set_by_value_mode(&mut tables, ParamMode::Value);
    assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
}

// ── Exponentiation eligibility ───────────────────────────
//
// Both same-typed scalar forms are emitted by the walker. Mixed forms stay
// structurally ineligible as a final verifier-safety boundary even though
// the type checker rejects them first.

// p1. an i64 power is claimed by the LIR walker.
#[test]
fn p1_scalar_i64_pow_is_lir_eligible() {
    assert!(eligible(
        "fn f(a: i64, b: i64) -> i64 { return a ** b; }",
        "f",
        &["f"]
    ));
}

// p2. an f64 power is claimed by the generated native kernel.
#[test]
fn p2_scalar_f64_pow_is_lir_eligible() {
    assert!(eligible(
        "fn f(a: f64, b: f64) -> f64 { return a ** b; }",
        "f",
        &["f"]
    ));
}

// p2b. a mixed power stays out too, from either side, and the rest of the
// function is what drops with it — no partial claim.
#[test]
fn p2b_mixed_float_pow_is_not_lir_eligible() {
    assert!(!eligible(
        "fn f(a: f64, b: i64) -> f64 { return a ** b; }",
        "f",
        &["f"]
    ));
    assert!(!eligible(
        "fn f(a: i64, b: f64) -> f64 { return a ** b; }",
        "f",
        &["f"]
    ));
}

// p2c. f64 power and ordinary float arithmetic can coexist on the walker.
#[test]
fn p2c_float_pow_and_float_arithmetic_are_lir_eligible() {
    assert!(eligible(
        "fn f(a: f64, b: f64, c: i64) -> i64 { let x = a ** b; return c + 1; }",
        "f",
        &["f"]
    ));
    assert!(eligible(
        "fn f(a: f64, b: f64, c: i64) -> i64 { let x = a * b; return c ** 2; }",
        "f",
        &["f"]
    ));
}

// p3. a right-associative chain is claimed as a whole.
#[test]
fn p3_pow_chain_is_lir_eligible() {
    assert!(eligible(
        "fn f(a: i64, b: i64, c: i64) -> i64 { return a ** b ** c; }",
        "f",
        &["f"]
    ));
}

// p4. a power mixed with the other arithmetic operators is claimed.
#[test]
fn p4_pow_mixed_with_arithmetic_is_lir_eligible() {
    assert!(eligible(
        "fn f(a: i64, b: i64) -> i64 { return a * b ** 2 + 1; }",
        "f",
        &["f"]
    ));
}

// p5. a power on String operands is NOT claimed — the walker emits only
// `+`, `==` and `!=` for strings, so `**` there must be rejected rather than
// reach `emit_lir_binop` (which has no string path at all).
#[test]
fn p5_string_pow_is_not_lir_eligible() {
    assert!(!eligible(
        "fn f(a: String, b: String) -> String { return a ** b; }",
        "f",
        &["f"]
    ));
}

// ---------------------------------------------------------------------
// — `Map<K, V>`, `FrozenArray<T>` and `FrozenMap<K, V>` in
// the LIR walker, plus the array methods that were still missing.
//
// Map keys follow the runtime `MapKey::Int | Str` representation: String
// keys carry the reference flag; i64, bool, and f64 keys occupy one raw
// word. Other key types fail validation. `get` returns the Option
// representation selected for the map's value type.
//
// Perspectives c01-c24 below are the *eligibility* half. The emitted-code
// half — LIR-on/off differentials, including under `WILLOW_GC_STRESS=alloc`
// — lives in `tests/integration/codegen.rs`, because rooting discipline is
// not something the predicate can observe.
//
// c01. a `Map<String, i64>` parameter with `len()` is eligible
// c02. an `i64` key and a GC-managed value is eligible
// c03. `Map::new()` + `insert` is eligible despite its `Map<Void, Void>`
// c04. `contains` is eligible
// c05. `toString` on a renderable value type is eligible
// c06. `toString` on a non-renderable value type is rejected
// c07. `freeze` on a map is eligible
// c08. `FrozenMap` answers `len` and `contains`
// c09. `FrozenArray` answers `len`
// c10. indexing a `FrozenArray` is eligible (it lowers like an array read)
// c11. `freeze` on an array is eligible
// c12. `Map::get` returns the value type's Option representation
// c13. `FrozenMap::get` uses the same Option representation
// c14. a `bool` key uses the raw-word MapKey representation
// c15. an `f64` key uses its raw bits as the key word
// c16. unsupported value layouts are rejected
// c17. nested maps are eligible: the value type is checked recursively
// c18. an `Array<T>` value type is eligible
// c19. a `FrozenArray` of class elements is eligible
// c20. `Map<Void, Void>` is NOT a declarable storage type — the exemption
//      is scoped to the `Map::new()` node alone
// c21. `is_fresh_empty_map` accepts only that node, not any `Map<Void,Void>`
// c22. a map and a frozen map are not interchangeable representations
// c23. an array and a frozen array are not interchangeable either
// c24. a non-collection generic (`Option`, `Range`, `Task`, a user generic)
//      is not mistaken for a collection

/// Registration tables for a program that declares no items, for tests that
/// ask the type predicate directly instead of through a function body.
fn empty_tables() -> TestTables {
    let tokens = Lexer::new("fn f() {}").tokenize().expect("lex");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "{errs:?}");
    TestTables::build(&program, &["f"], &[])
}

fn map_ty(key: Type, value: Type) -> Type {
    Type::Generic("Map".to_string().into(), vec![key, value])
}

fn frozen_map_ty(key: Type, value: Type) -> Type {
    Type::Generic("FrozenMap".to_string().into(), vec![key, value])
}

fn frozen_array_ty(elem: Type) -> Type {
    Type::Generic("FrozenArray".to_string().into(), vec![elem])
}

const MAP_IMPORT: &str = "import std::collections::Map;";

// c01. the base case: a map parameter, read for its length
#[test]
fn c01_map_parameter_len_eligible() {
    let src = format!("{MAP_IMPORT} fn f(m: Map<String, i64>) -> i64 {{ return m.len(); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// c02. the other admitted key type, with a GC-managed value: the value goes
// through the same store discipline as any other reference.
#[test]
fn c02_int_key_and_string_value_eligible() {
    let src = format!("{MAP_IMPORT} fn f(m: Map<i64, String>) -> i64 {{ return m.len(); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// c03. `Map::new()` types as `Map<Void, Void>` — the empty map really is
// untyped, since the runtime records nothing until the first insert. The
// walker exempts that one node so a `let` can still be claimed.
#[test]
fn c03_fresh_empty_map_eligible() {
    let src = format!(
        "{MAP_IMPORT} fn f() -> i64 {{ let m: Map<String, i64> = Map::new(); \
             m.insert(\"a\", 1); return m.len(); }}"
    );
    assert!(eligible_checked(&src, "f", &["f"]));
}

// c04. `contains` answers a `bool` from the same key ABI as `insert`
#[test]
fn c04_map_contains_eligible() {
    let src =
        format!("{MAP_IMPORT} fn f(m: Map<String, i64>) -> bool {{ return m.contains(\"a\"); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// c05. `toString` renders in the runtime, which knows the four scalar and
// string value kinds
#[test]
fn c05_map_to_string_eligible() {
    let src =
        format!("{MAP_IMPORT} fn f(m: Map<String, i64>) -> String {{ return m.toString(); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// c06. a value kind the runtime cannot render must not be passed as kind
// `0`, which would print a pointer as an `i64`. The checker owns the first
// line of defence here (E1402 rejects the source outright), so the walker's
// own guard is asked directly — it is what keeps the emitter's
// `collection_elem_kind(..).expect(..)` honest if that check ever moves.
#[test]
fn c06_map_to_string_unrenderable_value_ineligible() {
    let tables = empty_tables();
    let span = crate::diagnostics::Span::dummy();
    let to_string = |value: Type| HirExpr {
        kind: HirExprKind::MethodCall {
            object: Box::new(HirExpr {
                kind: HirExprKind::Var("m".to_string()),
                ty: map_ty(Type::String, value),
                span,
            }),
            method: "toString".to_string(),
            args: Vec::new(),
            super_class: None,
        },
        ty: Type::String,
        span,
    };
    let int_map = map_ty(Type::String, Type::I64);
    let array_map = map_ty(Type::String, Type::Array(Box::new(Type::I64)));
    let renderable = to_string(Type::I64);
    let unrenderable = to_string(Type::Array(Box::new(Type::I64)));
    tables.with_ctx(|ctx| {
        let names: HashMap<&str, Cow<'_, Type>> = HashMap::from([("m", Cow::Borrowed(&int_map))]);
        assert!(supported_expr(&renderable, ctx, &names));
        let names: HashMap<&str, Cow<'_, Type>> = HashMap::from([("m", Cow::Borrowed(&array_map))]);
        assert!(!supported_expr(&unrenderable, ctx, &names));
    });
}

// c07. `freeze` copies into a `FrozenMap<K, V>` over the SAME pair
#[test]
fn c07_map_freeze_eligible() {
    let src = format!(
        "{MAP_IMPORT} fn f(m: Map<String, i64>) -> i64 {{ let g = m.freeze(); \
             return g.len(); }}"
    );
    assert!(eligible_checked(&src, "f", &["f"]));
}

// c08. a frozen map is the same runtime object, so its reads lower to the
// same calls as the mutable one's
#[test]
fn c08_frozen_map_reads_eligible() {
    let src = "fn f(m: FrozenMap<String, i64>) -> i64 { if m.contains(\"a\") { return m.len(); } \
                   return 0; }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// c09. `len` is the whole declared surface of a frozen array
#[test]
fn c09_frozen_array_len_eligible() {
    let src = "fn f(xs: FrozenArray<i64>) -> i64 { return xs.len(); }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// c10. indexing a frozen array is an `Index`, not a method call — the HIR
// lowering had to learn the element type of a non-`Array` handle for this
// to reach the walker at all.
#[test]
fn c10_frozen_array_index_eligible() {
    let src = "fn f(xs: FrozenArray<i64>) -> i64 { return xs[0]; }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// c11. the array side of the same pair
#[test]
fn c11_array_freeze_eligible() {
    let src = "fn f(xs: Array<i64>) -> i64 { let ys = xs.freeze(); return ys.len(); }";
    assert!(eligible_checked(
        &format!("import std::collections::Array; {src}"),
        "f",
        &["f"]
    ));
}

// c12. `get` yields `Option<V>`, which the walker gained a representation
// for — so the call is now claimed rather than costing
// the function its LIR compilation.
#[test]
fn c12_map_get_eligible() {
    let src = format!(
        "{MAP_IMPORT} fn f(m: Map<String, i64>) -> i64 {{ \
             return m.get(\"a\").unwrap_or(0); }}"
    );
    assert!(eligible_checked(&src, "f", &["f"]));
    // A generic interface remains representable when nested in the map's
    // Option result.
    let generic_interface = format!(
        "{MAP_IMPORT} interface Boxed<T> {{ fn get(self) -> T; }} \
             fn f(m: Map<String, Boxed<String>>) -> i64 {{ \
             let b = m.get(\"a\"); return m.len(); }}"
    );
    assert!(eligible_checked(&generic_interface, "f", &["f"]));
}

// c13. the frozen kind is claimed on the same terms
#[test]
fn c13_frozen_map_get_eligible() {
    let src = "fn f(m: FrozenMap<String, i64>) -> i64 { return m.get(\"a\").unwrap_or(0); }";
    assert!(eligible_checked(src, "f", &["f"]));
    let control = "fn f(m: FrozenMap<String, i64>) -> i64 { return m.len(); }";
    assert!(eligible_checked(control, "f", &["f"]));
}

// c14. a `bool` key is one word the runtime stores verbatim, so it is in
// the subset like every other scalar key.
#[test]
fn c14_bool_key_eligible() {
    let src = format!("{MAP_IMPORT} fn f(m: Map<bool, i64>) -> i64 {{ return m.len(); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// c15. so is an `f64` key: the flag says "not a reference" and the bits go
// in as the key word, preserving its runtime representation.
#[test]
fn c15_float_key_eligible() {
    let src = format!("{MAP_IMPORT} fn f(m: Map<f64, i64>) -> i64 {{ return m.len(); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// c15b. a REFERENCE key that is not a `String` is the one the runtime
// cannot read: it would take the inner map's pointer for a `WillowString`.
#[test]
fn c15b_reference_key_other_than_string_ineligible() {
    let src =
        format!("{MAP_IMPORT} fn f(m: Map<Map<String, i64>, i64>) -> i64 {{ return m.len(); }}");
    assert!(!eligible_checked(&src, "f", &["f"]));
}

// c16. a generic interface is an admitted GC value and can be a map value.
#[test]
fn c16_generic_interface_value_type_eligible() {
    let src = format!(
        "{MAP_IMPORT} interface Boxed<T> {{ fn get(self) -> T; }} \
             fn f(m: Map<String, Boxed<String>>) -> i64 {{ return m.len(); }}"
    );
    assert!(eligible_checked(&src, "f", &["f"]));

    // The control: the same map over a type the walker does represent, so
    // this cannot pass by rejecting `Map` wholesale.
    let control = format!("{MAP_IMPORT} fn f(m: Map<String, String>) -> i64 {{ return m.len(); }}");
    assert!(eligible_checked(&control, "f", &["f"]));
}

// c17. the value check recurses: a map of maps is admitted because the
// inner map is itself an admitted storage type.
#[test]
fn c17_nested_map_eligible() {
    let src =
        format!("{MAP_IMPORT} fn f(m: Map<String, Map<String, i64>>) -> i64 {{ return m.len(); }}");
    assert!(eligible_checked(&src, "f", &["f"]));
}

// c18. an `Array<T>` value type is a plain GC handle in the slot
#[test]
fn c18_array_value_type_eligible() {
    let src = format!(
        "{MAP_IMPORT} import std::collections::Array; \
             fn f(m: Map<String, Array<i64>>) -> i64 {{ return m.len(); }}"
    );
    assert!(eligible_checked(&src, "f", &["f"]));
}

// c19. a frozen array of SIMPLE class elements, like the mutable one
#[test]
fn c19_frozen_array_of_class_elements_eligible() {
    let src = "class Item { pub name: String; } \
                   fn f(xs: FrozenArray<Item>) -> i64 { return xs.len(); }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// c20. the `Map<Void, Void>` exemption is scoped to one expression node: as
// a declared storage type it is still rejected, which is what makes the
// exemption sound — no parameter, local or return can ever have this type.
#[test]
fn c20_void_map_is_not_a_storage_type() {
    let tables = empty_tables();
    tables.with_ctx(|ctx| {
        assert!(!ctx.supported_type(&map_ty(Type::Void, Type::Void)));
        assert!(!ctx.supported_type(&map_ty(Type::String, Type::Void)));
        assert!(!ctx.supported_type(&frozen_array_ty(Type::Void)));
        // the control: the same shapes with real arguments are fine
        assert!(ctx.supported_type(&map_ty(Type::String, Type::I64)));
        assert!(ctx.supported_type(&frozen_map_ty(Type::I64, Type::String)));
        assert!(ctx.supported_type(&frozen_array_ty(Type::I64)));
    });
}

// c21. the exemption keys on the `Map::new()` CALL, not on the type: a
// variable that somehow carried `Map<Void, Void>` is not exempt.
#[test]
fn c21_empty_map_exemption_is_node_scoped() {
    let void_map = map_ty(Type::Void, Type::Void);
    let span = crate::diagnostics::Span::dummy();
    let fresh = HirExpr {
        kind: HirExprKind::StaticCall {
            class: "Map".to_string().into(),
            method: "new".to_string(),
            args: Vec::new(),
        },
        ty: void_map.clone(),
        span,
    };
    assert!(is_fresh_empty_map(&fresh));

    // same type, different node
    let var = HirExpr {
        kind: HirExprKind::Var("m".to_string()),
        ty: void_map.clone(),
        span,
    };
    assert!(!is_fresh_empty_map(&var));

    // same node, a type that is not the empty map
    let typed = HirExpr {
        kind: HirExprKind::StaticCall {
            class: "Map".to_string().into(),
            method: "new".to_string(),
            args: Vec::new(),
        },
        ty: map_ty(Type::String, Type::I64),
        span,
    };
    assert!(!is_fresh_empty_map(&typed));

    // and a different static call with the empty-map type is not exempt
    let other = HirExpr {
        kind: HirExprKind::StaticCall {
            class: "Other".to_string().into(),
            method: "new".to_string(),
            args: Vec::new(),
        },
        ty: void_map,
        span,
    };
    assert!(!is_fresh_empty_map(&other));
}

// c22. a map and a frozen map share a runtime object but are DIFFERENT
// types: a store from one into the other is not a representation match, or
// `freeze` would be a no-op the checker never sanctioned.
#[test]
fn c22_map_and_frozen_map_are_distinct_representations() {
    let m = map_ty(Type::String, Type::I64);
    let f = frozen_map_ty(Type::String, Type::I64);
    assert!(!assignable_repr(&m, &f));
    assert!(!assignable_repr(&f, &m));
    assert!(assignable_repr(&m, &m));
    // the type arguments are part of the representation, too
    assert!(!assignable_repr(&m, &map_ty(Type::String, Type::String)));
    assert!(!assignable_repr(&m, &map_ty(Type::I64, Type::I64)));
}

// c23. the same for the array pair, in both directions
#[test]
fn c23_array_and_frozen_array_are_distinct_representations() {
    let a = Type::Array(Box::new(Type::I64));
    let f = frozen_array_ty(Type::I64);
    assert!(!assignable_repr(&a, &f));
    assert!(!assignable_repr(&f, &a));
    assert!(assignable_repr(&f, &f));
    assert!(!assignable_repr(&f, &frozen_array_ty(Type::String)));
}

// c24. `lir_collection` decides by BUILTIN IDENTITY, not by generic shape,
// so no other generic — and no user generic — is read as a collection.
#[test]
fn c24_other_generics_are_not_collections() {
    // Not a collection AND not in the subset at all.
    for ty in [
        // `Range<i64>` is in the subset as of; every
        // OTHER instantiation of the name is still out, which is what
        // makes the point that the name alone decides nothing.
        Type::Generic("Range".to_string().into(), vec![Type::String]),
        Type::Generic("Holder".to_string().into(), vec![Type::I64]),
    ] {
        assert!(lir_collection(&ty).is_none(), "{ty:?} is not a collection");
        let tables = empty_tables();
        tables.with_ctx(|ctx| assert!(!ctx.supported_type(&ty), "{ty:?} is not storable"));
    }
    let channel = Type::Generic("Channel".to_string().into(), vec![Type::I64]);
    let task = Type::Generic("Task".to_string().into(), vec![Type::I64]);
    assert!(lir_collection(&channel).is_none());
    assert!(lir_collection(&task).is_none());
    let tables = empty_tables();
    tables.with_ctx(|ctx| {
        assert!(ctx.supported_type(&channel));
        assert!(ctx.supported_type(&task));
    });
    // `Option`/`Result` are storable as of, but they are
    // ENUMS, not collections: nothing may route them into the map/array
    // emission paths.
    for ty in [
        Type::Generic("Option".to_string().into(), vec![Type::I64]),
        Type::Generic("Result".to_string().into(), vec![Type::I64, Type::String]),
    ] {
        assert!(lir_collection(&ty).is_none(), "{ty:?} is not a collection");
        let tables = empty_tables();
        tables.with_ctx(|ctx| {
            assert!(ctx.supported_type(&ty), "{ty:?} is storable");
            assert!(ctx.supported_enum_type(&ty), "{ty:?} is an enum instance");
        });
    }
    // A collection name carrying the WRONG number of arguments resolves to
    // the builtin id — name resolution is by name — but has no admitted
    // shape, so nothing downstream can take it for a usable collection.
    let tables = empty_tables();
    for ty in [
        Type::Named("Map".to_string().into()),
        Type::Generic("Map".to_string().into(), vec![Type::String]),
        Type::Generic("FrozenArray".to_string().into(), vec![Type::I64, Type::I64]),
    ] {
        tables.with_ctx(|ctx| assert!(!ctx.supported_type(&ty), "{ty:?} is not storable"));
    }
    // the controls: the three that ARE collections
    assert!(matches!(
        lir_collection(&map_ty(Type::String, Type::I64)),
        Some((LirCollection::Map, _))
    ));
    assert!(matches!(
        lir_collection(&frozen_map_ty(Type::String, Type::I64)),
        Some((LirCollection::FrozenMap, _))
    ));
    assert!(matches!(
        lir_collection(&frozen_array_ty(Type::I64)),
        Some((LirCollection::FrozenArray, _))
    ));
}

/// Rewrite the declared mode of `Mode::by_value`'s single parameter,
/// leaving its type alone.
fn set_by_value_mode(tables: &mut TestTables, mode: ParamMode) {
    let methods = tables
        .iface_methods
        .get_mut("Mode")
        .expect("interface Mode");
    let slot = methods
        .iter()
        .position(|(n, _, _, _)| n == "by_value")
        .expect("by_value slot");
    methods[slot].2 = vec![mode];
}

// ── enum values and `match` ────────────────────────────
//
// Perspectives m01-m28. The differential behaviour — that a LIR-compiled
// enum program retained the former AST emitter's behavior, including under
// WILLOW_GC_STRESS=alloc — is pinned in tests/integration/codegen.rs; these
// pin the validation boundary that every emitted body must satisfy.

/// The two enums nearly every perspective below needs: one with no payload
/// anywhere (a bare `i64` tag) and one where a payload exists (so EVERY
/// value of it is a `[tag | payload…]` heap object).
const ENUMS: &str = "\
enum Color { Red, Green, Blue }
enum Shape { Nothing, Circle(i64), Rect(i64, i64), Labeled(String, f64) }
";

fn enum_src(body: &str) -> String {
    format!("{ENUMS}{body}")
}

/// Assert that `name` is REFUSED — and that it was refused for a reason,
/// not because the function never reached the lowered IR at all. A plain
/// `!eligible_checked(..)` would pass vacuously if the source stopped
/// compiling for an unrelated reason, which is exactly how a boundary test
/// rots into a test of nothing.
fn refused(src: &str, name: &str, fns: &[&str]) {
    let (f, tables) = lir_fn_and_tables(src, name, fns);
    assert!(
        tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)),
        "`{name}` must be rejected as unsupported lowered IR"
    );
}

// m01. the base case: a fieldless enum is constructed by its qualified name
// and matched by variant. Nothing here loads a tag — the value IS the tag.
#[test]
fn m01_fieldless_enum_construction_and_match_eligible() {
    let src = enum_src(
        "fn f(c: Color) -> i64 {
                return match c {
                    Color::Red => 1,
                    Color::Green => 2,
                    _ => 3
                };
            }
            fn g() -> Color { return Color::Blue; }",
    );
    assert!(eligible_checked(&src, "f", &[]));
    assert!(eligible_checked(&src, "g", &[]));
}

// m02. a payload variant: construction takes arguments and the pattern
// destructures them, which is the heap-object half of the representation
// rule.
#[test]
fn m02_payload_enum_construction_and_tuple_pattern_eligible() {
    let src = enum_src(
        "fn f(s: Shape) -> i64 {
                return match s {
                    Shape::Rect(w, h) => w * h,
                    Shape::Circle(r) => r,
                    _ => 0
                };
            }
            fn g() -> Shape { return Shape::Rect(2, 3); }",
    );
    assert!(eligible_checked(&src, "f", &[]));
    assert!(eligible_checked(&src, "g", &[]));
}

// m03. a GENERIC enum is in the subset when the use site is concrete: the
// scrutinee's type ARGUMENTS instantiate the declared placeholder payload,
// and eligibility performs exactly the substitution
// `resolve_variant_payload_types` performs at emission.
#[test]
fn m03_generic_user_enum_eligible() {
    let src = "enum Holder<T> { Empty, Full(T) }
            fn f(h: Holder<i64>) -> i64 {
                return match h {
                    Holder::Full(v) => v,
                    _ => 0
                };
            }
            fn g() -> Holder<i64> { return Holder::Full(1); }";
    assert!(eligible_checked(src, "f", &[]));
    assert!(eligible_checked(src, "g", &[]));

    // A generic interface is also a supported concrete payload.
    let generic_interface = "enum Holder<T> { Empty, Full(T) }
            interface Boxed<U> { fn get(self) -> U; }
            fn f(h: Holder<Boxed<String>>) -> i64 {
                return match h {
                    Holder::Full(b) => 1,
                    _ => 0
                };
            }";
    assert!(eligible_checked(generic_interface, "f", &[]));
}

#[test]
fn generic_enum_changing_recursive_arguments_are_opaque_references() {
    let source =
        "import std::collections::Array; enum Grow<T> { End, Next(Grow<Array<T>>) } fn f() {}";
    let (_, tables) = checked_lowering(source, &[]);
    tables.with_ctx(|ctx| {
        assert!(ctx.supported_enum_type(&Type::Generic("Grow".into(), vec![Type::I64])));
        assert!(!ctx.supported_enum_type(&Type::Named("Grow".into())));
        assert!(!ctx.supported_enum_type(&Type::Generic("Grow".into(), vec![])));
    });
}

#[test]
fn generic_enum_active_definition_does_not_hide_unsupported_arguments() {
    let (_, tables) = checked_lowering("fn f() {}", &[]);
    tables.with_ctx(|ctx| {
        let bad = Type::Generic(
            "Option".into(),
            vec![Type::Generic(
                "Option".into(),
                vec![Type::Named("Missing".into())],
            )],
        );
        let mut open = SupportedTypes {
            active_enums: HashMap::from([("Option".into(), 1)]),
            ..SupportedTypes::default()
        };
        assert!(!ctx.supported_enum_inner(&bad, &mut open));
        assert!(!ctx.supported_enum_inner(&Type::Generic("Option".into(), vec![]), &mut open));
    });
}

#[test]
fn generic_enum_sibling_instantiations_validate_each_payload() {
    let (_, tables) = checked_lowering(
        "import std::collections::Array; enum Items<T> { Values(Array<T>) } fn f() {}",
        &[],
    );
    tables.with_ctx(|ctx| {
        let mut open = SupportedTypes::default();
        assert!(
            ctx.supported_enum_inner(&Type::Generic("Items".into(), vec![Type::I64]), &mut open)
        );
        assert!(!open.active_enums.contains_key(&TypeId::from("Items")));
        // Void itself is supported, but Array<void> is not: this checks
        // payload validation rather than only validation of arguments.
        assert!(
            !ctx.supported_enum_inner(&Type::Generic("Items".into(), vec![Type::Void]), &mut open)
        );
    });
}

#[test]
fn generic_enum_recursive_fanout_looks_up_definition_once() {
    for count in [1, 16, 256] {
        let variants: String = (0..count)
            .map(|index| format!(", Next{index}(Grow<Array<T>> )"))
            .collect();
        let source =
            format!("import std::collections::Array; enum Grow<T> {{ End{variants} }} fn f() {{}}");
        let (_, tables) = checked_lowering(&source, &[]);
        tables.with_ctx(|base| {
            let lookups = std::cell::Cell::new(0usize);
            let enum_def = |name: &TypeId| {
                lookups.set(lookups.get() + 1);
                (base.enum_def)(name)
            };
            let ctx = LirTypeCtx {
                enum_def: &enum_def,
                ..*base
            };
            assert!(ctx.supported_enum_type(&Type::Generic("Grow".into(), vec![Type::I64])));
            assert_eq!(lookups.get(), 1, "recursive payloads={count}");
            eprintln!(
                "WILLOW_GENERIC_AUDIT {}",
                serde_json::json!({
                    "case": "enum_recursion",
                    "variants": count + 1,
                    "recursive_payloads": count,
                    "lookups": lookups.get(),
                })
            );
        });
    }
}

// m04. `Option<T>` over a scalar is the ordinary `[tag | payload]` heap
// object; over a GC payload it is the pointer niche, where `Some(x)` IS `x`
// and `None` is null. Both are emittable, so both are in.
#[test]
fn m04_option_match_eligible() {
    let boxed = "fn f(x: Option<i64>) -> i64 {
                return match x {
                    Some(v) => v,
                    None => -1
                };
            }";
    assert!(eligible_checked(boxed, "f", &[]));

    let niche = "fn f(x: Option<String>) -> String {
                return match x {
                    Some(v) => v,
                    None => \"\"
                };
            }";
    assert!(eligible_checked(niche, "f", &[]));
}

// m05. the same for `Result`, which is generic in two parameters — so a
// wrong-arity substitution would silently mis-slot the payload.
#[test]
fn m05_result_match_eligible() {
    let src = "fn f(r: Result<i64, String>) -> i64 {
                return match r {
                    Ok(v) => v,
                    Err(e) => -1
                };
            }";
    assert!(eligible_checked(src, "f", &[]));
}

// m06. a block-bodied arm may declare a local
// and then leave through it. The arm is bracketed, so the binding is scoped
// to it.
#[test]
fn m06_block_bodied_arm_with_a_let_eligible() {
    let src = enum_src(
        "fn f(c: Color) -> i64 {
                return match c {
                    Color::Red => { let t = 1; return t; },
                    _ => 2
                };
            }",
    );
    assert!(eligible_checked(&src, "f", &[]));
}

// m07. and a block arm that declares a local and then uses it — the same
// body shape without the departure.
#[test]
fn m07_arm_declaring_a_local_eligible() {
    let src = enum_src(
        "fn f(c: Color) {
                match c {
                    Color::Red => { let x = 1; println(x); },
                    _ => println(\"other\")
                }
            }",
    );
    assert!(eligible_checked(&src, "f", &[]));
}

// m08. a downcast pattern tests a runtime type
// id through an interface box rather than a tag, and the walker emits it:
// the scrutinee's word 0 IS the concrete object, so the test is an exact
// `type_id` compare against the arm's class and the binding is that object,
// unboxed. Exact rather than "is a descendant of" — a descendant of the
// arm's class does not match it.
#[test]
fn m08_class_downcast_pattern_eligible() {
    let src = "interface Speaker { fn speak(self) -> String; }
            class Dog implements Speaker { pub fn speak(self) -> String { return \"woof\"; } }
            fn f(s: Speaker) -> String {
                return match s {
                    Dog(d) => d.speak(),
                    _ => \"other\"
                };
            }";
    assert!(eligible_checked(src, "f", &[]));
}

// m08b. the downcast needs the arm class's `type_id` to compare against.
// With the id table emptied there is nothing to compare and the `match`
// fails validation rather than inventing a constant.
#[test]
fn m08b_class_downcast_needs_a_type_id() {
    let src = "interface Speaker { fn speak(self) -> String; }
            class Dog implements Speaker { pub fn speak(self) -> String { return \"woof\"; } }
            fn f(s: Speaker) -> String {
                return match s {
                    Dog(d) => d.speak(),
                    _ => \"other\"
                };
            }";
    let (f, mut tables) = lir_fn_and_tables(src, "f", &[]);
    assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    tables.class_type_ids.clear();
    assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
}

// An unconditional CFG binding copies the scrutinee without comparing it.
// String content-pattern comparisons are still outside the pattern subset.
#[test]
fn m09_unconditional_string_scrutinee_eligible() {
    let src = "fn f(s: String) -> i64 {
                return match s {
                    other => 1
                };
            }";
    assert!(eligible_checked(src, "f", &[]));
}

// An unconditional class binding needs no object identity comparison.
#[test]
fn m10_unconditional_class_scrutinee_eligible() {
    let src = "class Cell { pub v: i64; }
            fn f(c: Cell) -> i64 {
                return match c {
                    other => other.v
                };
            }";
    assert!(eligible_checked(src, "f", &[]));
}

// m11. an `i64` scrutinee with literal arms: no tag load, the arm test
// compares the scrutinee word itself.
#[test]
fn m11_int_literal_scrutinee_eligible() {
    let src = "fn f(n: i64) -> i64 {
                return match n {
                    0 => 100,
                    1 => 200,
                    k => k * 3
                };
            }";
    assert!(eligible_checked(src, "f", &[]));
}

// m12. a `bool` scrutinee compares an `i8`, so the arm's expected constant
// has to be built at that width — a mismatch would be a verifier error, not
// a wrong answer.
#[test]
fn m12_bool_literal_scrutinee_eligible() {
    let src = "fn f(b: bool) -> i64 {
                return match b {
                    true => 1,
                    false => 0
                };
            }";
    assert!(eligible_checked(src, "f", &[]));
}

// m13. a binding pattern aliases the WHOLE scrutinee and always matches, so
// it both ends the arm chain and must be in scope for its own body.
#[test]
fn m13_binding_pattern_eligible_and_in_scope() {
    let src = enum_src(
        "fn f(s: Shape) -> i64 {
                return match s {
                    Shape::Circle(r) => r,
                    other => 0
                };
            }",
    );
    assert!(eligible_checked(&src, "f", &[]));
}

// m14. a binding is scoped to its OWN arm, and a later arm reading it never
// reaches the walker at all: HIR lowering refuses the program outright, so
// there is no lowered function for eligibility to claim. Pinning WHERE the
// refusal happens matters — if lowering ever starts admitting this, the
// `eligible_lenient` half below is what still keeps the walker out.
#[test]
fn m14_binding_does_not_leak_into_a_later_arm() {
    let src = enum_src(
        "fn f(s: Shape) -> i64 {
                return match s {
                    Shape::Circle(r) => r,
                    Shape::Rect(w, h) => r,
                    _ => 0
                };
            }",
    );
    let tokens = Lexer::new(&src).tokenize().expect("lex");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "{errs:?}");
    let (_, diags) = crate::ir::lower::lower_program(&program);
    assert!(
        diags.iter().any(|d| d.message.contains("unbound variable")),
        "the leaked binding must be caught in lowering: {diags:?}"
    );
    assert!(!eligible_lenient(&src, "f", &[]));

    // the control: the same shape with each binding used in its own arm is
    // admitted, so the refusal above is about SCOPE and nothing else
    let ok = enum_src(
        "fn f(s: Shape) -> i64 {
                return match s {
                    Shape::Circle(r) => r,
                    Shape::Rect(w, h) => w,
                    _ => 0
                };
            }",
    );
    assert!(eligible_checked(&ok, "f", &[]));
}

// m15. the positive control for m14: the same name used inside the arm that
// binds it is fine, and a second arm may reuse the name independently.
#[test]
fn m15_same_binding_name_in_two_arms_eligible() {
    let src = enum_src(
        "fn f(s: Shape) -> i64 {
                return match s {
                    Shape::Circle(v) => v,
                    Shape::Rect(v, h) => v + h,
                    _ => 0
                };
            }",
    );
    assert!(eligible_checked(&src, "f", &[]));
}

// m16. every arm feeds ONE result variable with no conversion inserted, so
// two different enums — and an enum against a class — are never
// representation-compatible even though both are `Type::Named`.
#[test]
fn m16_two_named_types_are_never_repr_compatible() {
    let color = Type::Named("Color".to_string().into());
    let shape = Type::Named("Shape".to_string().into());
    assert!(assignable_repr(&color, &color));
    assert!(!assignable_repr(&color, &shape));
    assert!(!assignable_repr(&shape, &color));
    assert!(!assignable_repr(
        &color,
        &Type::Named("Cell".to_string().into())
    ));
    // and a named type never matches a scalar, in either direction, even
    // when both are one machine word
    assert!(!assignable_repr(&color, &Type::I64));
    assert!(!assignable_repr(&Type::I64, &color));
}

// m17. a `match` in statement position produces no value. The walker still
// emits the arm chain and merge block; it just seeds and discards a
// `Void`-typed result.
#[test]
fn m17_statement_position_match_eligible() {
    let src = enum_src(
        "fn f(c: Color) {
                match c {
                    Color::Red => println(\"red\"),
                    _ => println(\"other\")
                }
            }",
    );
    assert!(eligible_checked(&src, "f", &[]));
}

// m18. a payload type is vetted like any other storage type: a `String` is
// in the subset, a generic `Option<i64>` payload is not — and one bad
// payload on ONE variant takes the whole enum out.
#[test]
fn m18_payload_types_are_vetted() {
    let ok = enum_src(
        "fn f(s: Shape) -> String {
                return match s {
                    Shape::Labeled(name, scale) => name,
                    _ => \"\"
                };
            }",
    );
    assert!(eligible_checked(&ok, "f", &[]));

    // Both `Option<i64>` and a generic interface are supported payloads.
    let nested = "enum Wrapped { Nothing, Boxed(Option<i64>) }
            fn f(w: Wrapped) -> i64 {
                return match w {
                    Wrapped::Nothing => 0,
                    _ => 1
                };
            }";
    assert!(eligible_checked(nested, "f", &[]));

    let generic_interface = "interface Boxed<T> { fn get(self) -> T; }
            enum Wrapped { Nothing, Full(Boxed<String>) }
            fn f(w: Wrapped) -> i64 {
                return match w {
                    Wrapped::Nothing => 0,
                    _ => 1
                };
            }";
    assert!(eligible_checked(generic_interface, "f", &[]));
}

// m19. a self-referential enum must terminate the payload walk rather than
// recursing forever — the `open` set is what makes the check total, and the
// enum is admitted rather than merely surviving the question.
#[test]
fn m19_self_referential_enum_terminates_and_is_admitted() {
    let src = "enum Chain { End, Link(i64, Chain) }
            fn f(c: Chain) -> i64 {
                return match c {
                    Chain::Link(v, rest) => v,
                    _ => 0
                };
            }";
    assert!(eligible_checked(src, "f", &[]));
    // mutual recursion through a second enum closes the other cycle shape
    let mutual = "enum Odd { Zero, Next(Even) }
            enum Even { One, Prev(Odd) }
            fn f(o: Odd) -> i64 {
                return match o {
                    Odd::Zero => 0,
                    _ => 1
                };
            }";
    assert!(eligible_checked(mutual, "f", &[]));
}

// m20. an enum field does not disqualify the class that holds it: the class
// stays "simple", so `new` and field reads stay on the walker.
#[test]
fn m20_enum_as_class_field_keeps_the_class_simple() {
    let src = enum_src(
        "class Course { pub facing: Color; pub outline: Shape; }
            fn f() -> i64 {
                let c = new Course(Color::Red, Shape::Circle(2));
                return match c.outline {
                    Shape::Circle(r) => r,
                    _ => 0
                };
            }",
    );
    assert!(eligible_checked(&src, "f", &[]));
}

// m21. enums are array elements like any other value; a payload enum's
// elements are GC references the array has to trace, and the element `is_ref`
// flag comes from the enum-aware `is_gc_managed`.
#[test]
fn m21_enum_array_elements_eligible() {
    let src = format!(
        "import std::collections::Array;
            {ENUMS}
            fn f(xs: Array<Shape>) -> i64 {{
                return match xs[0] {{
                    Shape::Circle(r) => r,
                    _ => 0
                }};
            }}
            fn g(xs: Array<Color>) -> i64 {{
                return match xs[0] {{
                    Color::Red => 1,
                    _ => 0
                }};
            }}"
    );
    assert!(eligible_checked(&src, "f", &[]));
    assert!(eligible_checked(&src, "g", &[]));
}

// m22. an enum crosses a call boundary in both directions — as a parameter
// and as a return type.
#[test]
fn m22_enum_parameter_and_return_eligible() {
    let src = enum_src(
        "fn make(n: i64) -> Shape { return Shape::Circle(n); }
            fn f(n: i64) -> i64 {
                return match make(n) {
                    Shape::Circle(r) => r,
                    _ => 0
                };
            }",
    );
    assert!(eligible_checked(&src, "f", &["make"]));
}

// m23. `supported_enum` and `supported_class` must not both claim a name.
// An interface registration wins, because interface VALUES are boxes.
#[test]
fn m23_a_name_registered_as_an_interface_is_not_a_supported_enum() {
    let src = enum_src("fn f(c: Color) -> i64 { return 0; }");
    let (_, mut tables) = checked_lowering(&src, &[]);
    tables.with_ctx(|ctx| {
        assert!(ctx.supported_enum("Color"));
        assert!(ctx.is_enum("Color"));
        assert!(ctx.supported_type(&Type::Named("Color".to_string().into())));
    });
    tables.interfaces.insert("Color".to_string());
    tables.with_ctx(|ctx| {
        assert!(!ctx.supported_enum("Color"));
        // still an enum by declaration — which is exactly why the class
        // path must keep excluding it
        assert!(ctx.is_enum("Color"));
    });
}

// m24. an undeclared name is not an enum, so nothing routes a module call
// or an unknown static call into enum construction.
#[test]
fn m24_unknown_names_are_not_enums() {
    let src = enum_src("fn f(c: Color) -> i64 { return 0; }");
    let (_, tables) = checked_lowering(&src, &[]);
    tables.with_ctx(|ctx| {
        for name in ["Cell", "math", "Map", "String"] {
            assert!(!ctx.is_enum(name), "{name} is not a declared enum");
            assert!(!ctx.supported_enum(name), "{name} is not a supported enum");
        }
        // `Option`/`Result` ARE declared (by the prelude), but only an
        // instantiation of one is a supported enum: the bare name supplies
        // no type arguments, so `enum_instance` refuses it and no
        // uninstantiated generic can reach construction or `match`.
        for name in ["Option", "Result"] {
            assert!(ctx.is_enum(name), "{name} is a prelude enum");
            assert!(
                !ctx.supported_enum(name),
                "bare `{name}` has no type arguments"
            );
        }
    });
}

// m25. arity is part of the pattern's contract: a destructuring pattern
// whose binding count differs from the variant's payload count would read
// (or skip) a slot that does not exist. Like m14 this is refused during
// lowering, so the walker never sees it — and the `eligible_lenient` half
// is what still holds if lowering ever learns to represent it.
#[test]
fn m25_wrong_pattern_arity_ineligible() {
    let src = enum_src(
        "fn f(s: Shape) -> i64 {
                return match s {
                    Shape::Rect(w) => w,
                    _ => 0
                };
            }",
    );
    let tokens = Lexer::new(&src).tokenize().expect("lex");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "{errs:?}");
    let (_, diags) = crate::ir::lower::lower_program(&program);
    assert!(
        diags.iter().any(|d| d.message.contains("arity")),
        "a mis-arity pattern must be caught in lowering: {diags:?}"
    );
    assert!(!eligible_lenient(&src, "f", &[]));
}

// m26. the representation rule itself, read from the same place emission
// reads it: ANY payload variant makes EVERY value of the enum a heap
// object, and an enum with none is a bare tag that must NOT be rooted.
#[test]
fn m26_representation_follows_the_whole_enum_not_the_variant() {
    use super::super::EnumInfo;
    use crate::semantic::symbols::EnumVariantInfo;
    let variant = |name: &str, payloads: Vec<Type>, tag: i64| EnumVariantInfo {
        name: name.to_string(),
        payload_types: payloads,
        tag,
        declaration_span: crate::diagnostics::Span::dummy(),
    };
    let mut infos = TypeMap::new();
    infos.insert(
        "Color".to_string(),
        EnumInfo {
            name: "Color".into(),
            public: true,
            type_params: vec![],
            declaration_span: crate::diagnostics::Span::dummy(),
            variants: vec![variant("Red", vec![], 0), variant("Green", vec![], 1)],
        },
    );
    infos.insert(
        "Shape".to_string(),
        EnumInfo {
            name: "Shape".into(),
            public: true,
            type_params: vec![],
            declaration_span: crate::diagnostics::Span::dummy(),
            variants: vec![
                // the payload-less variant comes FIRST, so a per-variant
                // rule would get this backwards
                variant("Nothing", vec![], 0),
                variant("Circle", vec![Type::I64], 1),
            ],
        },
    );
    assert!(!is_gc_managed(
        &Type::Named("Color".to_string().into()),
        &infos
    ));
    assert!(is_gc_managed(
        &Type::Named("Shape".to_string().into()),
        &infos
    ));
    // and an undeclared named type is a class, which always is
    assert!(is_gc_managed(
        &Type::Named("Cell".to_string().into()),
        &infos
    ));
}

// m27. an interface-typed payload slot holds a BOX, and the box is built
// from the DECLARED payload type rather than the argument's own type — so
// a class argument is admitted only because the store position can convert.
#[test]
fn m27_interface_payload_is_eligible_and_boxed() {
    let src = "interface Named { fn describe(self) -> String; }
            class Marker implements Named {
                pub label: String;
                pub fn describe(self) -> String { return self.label; }
            }
            enum Tag { Untagged, Marked(Named) }
            fn make() -> Tag { return Tag::Marked(new Marker(\"m\")); }
            fn f(t: Tag) -> String {
                return match t {
                    Tag::Marked(n) => n.describe(),
                    _ => \"untagged\"
                };
            }";
    assert!(eligible_checked(src, "make", &[]));
    assert!(eligible_checked(src, "f", &[]));
    // a class value is NOT repr-compatible with the interface slot, which
    // is why the store position (and not an arm body) is where the box is
    // allowed to appear
    assert!(!assignable_repr(
        &Type::Named("Named".to_string().into()),
        &Type::Named("Marker".to_string().into())
    ));
}

// m28. a `match` may be an arm body of another `match`: the emitter is
// re-entrant, and the inner one's merge block leaves the builder where the
// outer one expects it.
#[test]
fn m28_nested_match_eligible() {
    let src = enum_src(
        "fn f(c: Color, s: Shape) -> i64 {
                return match c {
                    Color::Red => match s {
                        Shape::Circle(r) => r,
                        _ => 0
                    },
                    _ => match s {
                        Shape::Rect(w, h) => w * h,
                        _ => -1
                    }
                };
            }",
    );
    assert!(eligible_checked(&src, "f", &[]));
}

// ── `Option`, `Result` and `?` ──────────────────────
//
// Perspectives p01-p24. `Option` and `Result` are ordinary prelude enums,
// so the m-block above already covers "does the walker understand a generic
// enum". What these pin is the part that is NOT generic-enum machinery: the
// two representations an `Option` instance can have, the value-taking
// methods, `Map::get` (the one builtin that hands back an `Option`), and
// `?` — the only expression in the subset that leaves the function from the
// middle of another expression. The differential behaviour lives in
// tests/integration/codegen.rs; these pin the ELIGIBILITY boundary.

// p01. the representation split is per INSTANTIATION, not per enum:
// `Option<i64>` is a `[tag | payload]` object and `Option<String>` is the
// pointer niche. Both are emittable, so a function may mix them.
#[test]
fn p01_both_option_representations_eligible() {
    let src = "fn f(a: Option<i64>, b: Option<String>) -> i64 {
                return match a {
                    Some(v) => v,
                    None => match b { Some(_) => 1, None => 0 }
                };
            }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// p02. an `Option` of an `Option` is never the niche — the inner `None`
// would be indistinguishable from the outer one — and the walker still
// admits it, because `option_repr` is what decides, not the shape.
#[test]
fn p02_nested_option_eligible() {
    let src = "fn f(x: Option<Option<i64>>) -> i64 {
                return match x {
                    Some(inner) => inner.unwrap_or(-1),
                    None => -2
                };
            }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// p03. construction: the payload slot is vetted against the INSTANCE, so
// `Option::Some(1)` at type `Option<i64>` checks its argument against
// `i64`, not against the declaration's `T`.
#[test]
fn p03_option_construction_vets_the_instantiated_payload() {
    let src = "fn f() -> Option<i64> { return Option::Some(1); }
            fn g() -> Option<String> { return Option::Some(\"a\"); }";
    assert!(eligible_checked(src, "f", &["f", "g"]));
    assert!(eligible_checked(src, "g", &["f", "g"]));
}

// p04. the fieldless variant of a generic enum: HIR spells a bare `None`
// as a static-property read, and it has to instantiate the same way.
#[test]
fn p04_bare_none_is_a_variant_read() {
    let src = "fn f() -> Option<i64> { return None; }
            fn g() -> Option<String> { return Option::None; }";
    assert!(eligible_checked(src, "f", &["f", "g"]));
    assert!(eligible_checked(src, "g", &["f", "g"]));
}

// p05. `Result<void, E>::Ok()` takes ZERO arguments while the substituted
// payload list is `[void]`. Eligibility and emission both normalise that
// list away, so the arity check sees `0 == 0` and the object built is the
// one-word object required by the enum layout.
#[test]
fn p05_void_ok_payload_is_normalised_away() {
    let src = "fn f() -> Result<void, String> { return Ok(); }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// p06. a generic interface remains supported through an Option payload.
#[test]
fn p06_generic_interface_payload_is_eligible() {
    let src = "interface Boxed<T> { fn get(self) -> T; }
            fn f(x: Option<Boxed<String>>) -> i64 {
                return match x { Some(b) => 1, None => 0 };
            }
            fn g(x: Option<i64>) -> i64 {
                return match x { Some(v) => v, None => 0 };
            }";
    assert!(eligible_checked(src, "f", &["f", "g"]));
    assert!(eligible_checked(src, "g", &["f", "g"]));
}

// p07. the value-taking `Option` methods, all five of them. They are
// emitted inline off the same two words the `match` reads, so admitting
// `match` without them would be an arbitrary split.
#[test]
fn p07_option_value_methods_eligible() {
    let src = "fn a(x: Option<i64>) -> bool { return x.is_some(); }
            fn b(x: Option<i64>) -> bool { return x.is_none(); }
            fn c(x: Option<i64>) -> i64 { return x.unwrap(); }
            fn d(x: Option<i64>) -> i64 { return x.expect(\"boom\"); }
            fn e(x: Option<i64>) -> i64 { return x.unwrap_or(0); }";
    let fns = &["a", "b", "c", "d", "e"];
    for name in fns {
        assert!(eligible_checked(src, name, fns), "`{name}` must be in");
    }
}

// p08. the same for `Result`, including `unwrap_err`, which reads the
// SECOND type argument — a wrong-slot substitution would return the Ok
// payload's type here and the store check downstream would not notice.
#[test]
fn p08_result_value_methods_eligible() {
    let src = "fn a(r: Result<i64, String>) -> bool { return r.is_ok(); }
            fn b(r: Result<i64, String>) -> bool { return r.is_err(); }
            fn c(r: Result<i64, String>) -> i64 { return r.unwrap(); }
            fn d(r: Result<i64, String>) -> String { return r.unwrap_err(); }
            fn e(r: Result<i64, String>) -> i64 { return r.expect(\"boom\"); }
            fn g(r: Result<i64, String>) -> i64 { return r.unwrap_or(0); }";
    let fns = &["a", "b", "c", "d", "e", "g"];
    for name in fns {
        assert!(eligible_checked(src, name, fns), "`{name}` must be in");
    }
}

// p09. the value methods work over the niche representation too, where
// `unwrap` is the identity on the receiver rather than a payload load.
#[test]
fn p09_option_value_methods_over_the_niche_eligible() {
    let src = "fn a(x: Option<String>) -> bool { return x.is_some(); }
            fn b(x: Option<String>) -> String { return x.unwrap(); }
            fn c(x: Option<String>) -> String { return x.unwrap_or(\"\"); }";
    let fns = &["a", "b", "c"];
    for name in fns {
        assert!(eligible_checked(src, name, fns), "`{name}` must be in");
    }
}

// p10. the closure-taking combinators joined the same arm once function
// values existed. The method table is still a CLOSED
// list, so a name outside it falls through rather than being emitted as an
// unknown call.
#[test]
fn p10_closure_combinators_eligible() {
    let src = "fn f(x: Option<i64>) -> i64 { return x.map(|v: i64| v * 2).unwrap_or(0); }";
    assert!(eligible_checked(src, "f", &["f"]));
    // The list is closed against names the language does not have either,
    // so a future `Option::filter` cannot be emitted before it is written.
    let opt_i64 = Type::Generic("Option".to_string().into(), vec![Type::I64]);
    let pred = Type::Fn(vec![Type::I64], Box::new(Type::Bool));
    assert!(option_result_method(&opt_i64, "filter", &[pred]).is_none());
}

// p11. `unwrap_or`'s argument must have the payload's representation. A
// `String` default for an `Option<i64>` would be handed to the merge as a
// pointer in an integer slot.
#[test]
fn p11_unwrap_or_argument_type_is_checked() {
    let src = "fn f(x: Option<String>) -> String { return x.unwrap_or(\"d\"); }";
    assert!(eligible_checked(src, "f", &["f"]));

    // The arity is checked in the same place.
    let no_arg = "fn f(x: Option<i64>) -> i64 { return x.unwrap(); }";
    assert!(eligible_checked(no_arg, "f", &["f"]));
    let extra = "fn f(x: Option<i64>) -> bool { return x.is_some(); }";
    assert!(eligible_checked(extra, "f", &["f"]));
}

// p12. `Map::get` is the one builtin that hands back an `Option`, and the
// representation the runtime builds is chosen from the map's OWN value
// type — so the result type is checked against it rather than assumed.
#[test]
fn p12_map_get_yields_the_maps_own_option() {
    let src = "import std::collections::Map;
            fn f(m: Map<String, i64>) -> i64 { return m.get(\"k\").unwrap_or(-1); }
            fn g(m: Map<String, String>) -> String {
                return match m.get(\"k\") { Some(v) => v, None => \"\" };
            }";
    assert!(eligible_checked(src, "f", &["f", "g"]));
    assert!(eligible_checked(src, "g", &["f", "g"]));
}

// p13. a `get` whose result type is an `Option` over something OTHER than
// the map's value type is refused: the two would disagree about the niche.
// Built by rewriting the node's type, because the checker cannot produce
// this state from source.
#[test]
fn p13_map_get_result_must_match_the_value_type() {
    let src = "import std::collections::Map;
            fn f(m: Map<String, String>) -> Option<String> { return m.get(\"k\"); }";
    assert!(eligible_checked(src, "f", &["f"]));

    let (mut f, tables) = lir_fn_and_tables(src, "f", &["f"]);
    let Some(Terminator::Return(Some(v))) = f.blocks.last_mut().map(|b| &mut b.terminator) else {
        panic!("the function returns the `get` result");
    };
    let crate::ir::lowered::LirOperand::Local(result) = v else {
        panic!("materialized map result");
    };
    let forged = Type::Generic("Option".to_string().into(), vec![Type::I64]);
    f.locals[result.0 as usize].ty = forged.clone();
    f.return_type = forged;
    assert!(
        tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)),
        "a `get` typed over a different value type must be rejected"
    );
}

// p14. `?` on a `Result` whose error type already matches the enclosing
// function's: the failure path forwards the operand unchanged.
#[test]
fn p14_try_propagate_on_result_eligible() {
    let src = "fn src(n: i64) -> Result<i64, String> { return Result::Ok(n); }
            fn f(n: i64) -> Result<i64, String> {
                let v = src(n)?;
                return Result::Ok(v + 1);
            }";
    assert!(eligible_checked(src, "f", &["src", "f"]));
}

// p15. `?` on an `Option`, both directions across the representation
// boundary: niche operand into a boxed return and boxed operand into a
// niche return. The failure value is CONSTRUCTED for the destination in
// both, which is why neither is a special case of the other.
#[test]
fn p15_try_propagate_across_option_representations_eligible() {
    let src = "fn niche(n: i64) -> Option<String> { return Option::Some(\"a\"); }
            fn boxed(n: i64) -> Option<i64> { return Option::Some(n); }
            fn f(n: i64) -> Option<i64> {
                let s = niche(n)?;
                return Option::Some(1);
            }
            fn g(n: i64) -> Option<String> {
                let v = boxed(n)?;
                return Option::Some(\"b\");
            }";
    let fns = &["niche", "boxed", "f", "g"];
    assert!(eligible_checked(src, "f", fns));
    assert!(eligible_checked(src, "g", fns));
}

// p16. `?` with automatic error conversion: the operand's
// error type differs from the function's, so the failure path calls
// `into()` and re-wraps. Admitting it depends on `Result<i64, PortError>`
// being a supported type, which is what keeps the `into` dispatch a single
// direct call.
#[test]
fn p16_try_propagate_with_error_conversion_eligible() {
    let src = "class ConfigError { pub code: i64; }
            class PortError implements Into<ConfigError> {
                pub raw: i64;
                pub fn into(self) -> ConfigError { return new ConfigError(500); }
            }
            fn read(n: i64) -> Result<i64, PortError> { return Result::Ok(n); }
            fn f(n: i64) -> Result<i64, ConfigError> {
                let v = read(n)?;
                return Result::Ok(v);
            }";
    assert!(eligible_checked(src, "f", &["read", "f"]));
}

// p17. `?` on a `Result<void, E>` is IN. The `Ok()`
// object is the tag word alone — `enum_instance` normalizes the `void`
// payload away — so the success arm reads nothing rather than loading a
// word 1 that was never written. The control is the same shape carrying a
// real payload, which does load it.
#[test]
fn p17_try_propagate_on_a_void_result_eligible() {
    let src = "fn unit(n: i64) -> Result<void, String> { return Ok(); }
            fn valued(n: i64) -> Result<i64, String> { return Result::Ok(n); }
            fn f(n: i64) -> Result<i64, String> {
                unit(n)?;
                return Result::Ok(1);
            }
            fn g(n: i64) -> Result<i64, String> {
                let v = valued(n)?;
                return Result::Ok(v);
            }";
    let fns = &["unit", "valued", "f", "g"];
    assert!(eligible_checked(src, "f", fns));
    assert!(eligible_checked(src, "g", fns));
}

// p18. `?` propagates the rejection of its OPERAND: an unsupported inner
// expression cannot be laundered by wrapping it in a `?`.
#[test]
fn p18_try_propagate_inherits_its_operands_rejection() {
    let src = "fn f(n: i64) -> Result<i64, String> {
                let v = unknown(n)?;
                return Result::Ok(v);
            }
            fn unknown(n: i64) -> Result<i64, String> { return Result::Ok(n); }";
    // `unknown` is deliberately absent from the known-symbol set.
    refused(src, "f", &["f"]);
}

// p19. `?` inside a loop body is still just an expression: the early
// return leaves the loop and the function at once.
#[test]
fn p19_try_propagate_inside_a_loop_eligible() {
    let src = "fn step(n: i64) -> Result<i64, String> { return Result::Ok(n); }
            fn f(n: i64) -> Result<i64, String> {
                let mut total = 0;
                let mut i = 0;
                while i < n {
                    total = total + step(i)?;
                    i = i + 1;
                }
                return Result::Ok(total);
            }";
    assert!(eligible_checked(src, "f", &["step", "f"]));
}

// p20. Option<void> propagation now has explicit tag-based LIR control flow.
#[test]
fn p20_void_option_propagation_is_eligible() {
    let src = "fn unit(n: i64) -> Option<void> { return Some(); }
            fn f(n: i64) -> Option<i64> {
                unit(n)?;
                return Some(1);
            }";
    assert!(eligible_checked(src, "f", &["unit", "f"]));
}

// p21. `Option` and `Result` are enums by REGISTRATION, not by name. With
// the enum table emptied — the state a missing prelude registration would
// produce — nothing about them is assumed.
#[test]
fn p21_option_is_not_special_cased_by_name() {
    let src = "fn f(x: Option<i64>) -> i64 {
                return match x { Some(v) => v, None => 0 };
            }";
    let (f, mut tables) = lir_fn_and_tables(src, "f", &["f"]);
    assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    tables.enums.clear();
    assert!(
        tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)),
        "with no enum registered, `Option<i64>` must not be assumed"
    );
}

// p22. an unsupported generic remains rejected, so admitting
// `Type::Generic` for enums did not open the shape as a whole.
#[test]
fn p22_non_enum_generics_are_rejected() {
    let tables = empty_tables();
    for ty in [
        // A generic CLASS instantiation: the walker admits generic enums
        // and the builtin handles by NAME, never a `Type::Generic` for its
        // shape alone. Not `Future<i64>`: that one IS admitted as of
        //, as an opaque runtime pointer word.
        Type::Generic("Holder".to_string().into(), vec![Type::I64]),
        // Not `Range<i64>`: that one IS admitted as of.
        Type::Generic("Range".to_string().into(), vec![Type::String]),
    ] {
        tables.with_ctx(|ctx| {
            assert!(!ctx.supported_type(&ty), "{ty:?} is not an admitted enum");
        });
    }
}

// p22b. the opaque runtime-pointer generics ARE admitted, and their output
// type is vetted like any element type. `Future<void>` is
// the only instantiation `sleep`/`yield` can produce; the walker takes a
// declaration of any of them, since it only ever copies the word.
#[test]
fn p22b_future_is_admitted_and_vets_its_output() {
    let tables = empty_tables();
    tables.with_ctx(|ctx| {
        assert!(ctx.supported_type(&Type::Generic(
            "Future".to_string().into(),
            vec![Type::Void]
        )));
        assert!(ctx.supported_type(&Type::Generic(
            "Future".to_string().into(),
            vec![Type::String]
        )));
        assert!(
            !ctx.supported_type(&Type::Generic(
                "Future".to_string().into(),
                vec![Type::Generic("Holder".to_string().into(), vec![Type::I64])]
            )),
            "an output type outside the subset keeps the future out"
        );
    });
}

// p23. an `Option` in every storage position an ordinary value has: a
// class field, an array element, a parameter and a return.
#[test]
fn p23_options_in_ordinary_storage_positions_eligible() {
    let src = "import std::collections::Array;
            class Reading { pub value: Option<i64>; }
            fn field(r: Reading) -> i64 { return r.value.unwrap_or(0); }
            fn elem(xs: Array<Option<i64>>) -> i64 { return xs[0].unwrap_or(0); }
            fn pass(x: Option<String>) -> Option<String> { return x; }";
    let fns = &["field", "elem", "pass"];
    for name in fns {
        assert!(eligible_checked(src, name, fns), "`{name}` must be in");
    }
}

// p24. a recursive generic enum closes on itself at a concrete
// instantiation. Without keying the guard on the INSTANTIATED name this
// walk would not terminate, and `Option<Option<i64>>` (p02) proves the
// guard is not simply "stop at the enum name".
#[test]
fn p24_recursive_generic_enum_terminates() {
    let src = "enum List<T> { Cons(T, List<T>), Nil }
            fn f(l: List<i64>) -> i64 {
                return match l { List::Cons(h, t) => h, List::Nil => 0 };
            }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// ---------------------------------------------------------------------
// Rejection reasons.
//
// `lir_rejection_reason` is what the compile error prints, and it is
// also the single implementation of eligibility. Perspectives r1..r24:
// agreement with the predicate, each rejection site in the function-level
// scan (return type, by-ref param, param type, duplicate name, `let` type,
// store conversion, unresolvable assign target, non-array element store,
// unknown field layout, `void` return value, `defer`, static-field store,
// `super.init`), minimality of the blamed sub-expression, the two scopes
// the search must not descend into (lambda body, unbindable match arm),
// line attribution, and the naming of each expression form.
// ---------------------------------------------------------------------

// r1. an eligible function has no reason at all — the wrapper and the
// reason form are the same decision.
#[test]
fn r1_eligible_function_has_no_reason() {
    let src = "fn f(a: i64) -> i64 { let b = a * 2; return b + 1; }";
    assert_eq!(reason_of(src, "f", &["f"]), None);
}

// r2. the predicate and the reason can never disagree, over every source
// the surrounding tests use in both directions.
#[test]
fn r2_reason_agrees_with_predicate() {
    let cases: &[&str] = &[
        "fn f(a: i64) -> i64 { return a; }",
        "fn f() -> Range<i64> { return 0..3; }",
        "fn f(x: &mut i64) { x = x + 1; }",
        "class C { pub v: i64; } fn f(c: C) -> i64 { return c.v; }",
        "fn f() { let a = 1; while a < 2 { let a = 2; print(a); } }",
        "fn f() -> i64 { return g(); } fn g() -> i64 { return 1; }",
    ];
    for src in cases {
        let (p, tables) = checked_lowering(src, &["f", "g"]);
        for lf in &p.functions {
            let (supported, reason) = tables.with_ctx(|ctx| {
                (
                    lir_supported_function(lf, ctx),
                    lir_rejection_reason(lf, ctx),
                )
            });
            assert_eq!(
                supported,
                reason.is_none(),
                "`{}` disagreed: supported={supported} reason={reason:?}",
                lf.name
            );
        }
    }
}

// r3. an unsupported return type is named, since that is the one blocker
// no line number inside the body can point at.
#[test]
fn r3_return_type_reason_names_the_type() {
    let src = "import std::collections::Map; \
                   fn f() -> Map<Map<String, i64>, i64> { return Map::new(); }";
    assert_eq!(
        rejected(src, "f", &["f"]),
        "its return type `Map<Map<String, i64>, i64>` is outside the walker's subset"
    );
}

// r4. a by-reference parameter itself is no longer a rejection reason.
#[test]
fn r4_by_reference_parameter_has_no_rejection_reason() {
    let src = "fn f(x: &mut i64) { x = x + 1; }";
    let (f, tables) = lir_fn_and_tables(src, "f", &["f"]);
    assert!(tables.with_ctx(|ctx| lir_rejection_reason(&f, ctx).is_none()));
}

// r5. an unsupported parameter type names the parameter AND the type.
#[test]
fn r5_parameter_type_reason_names_both() {
    let src = "import std::collections::Map; \
                   fn f(m: Map<Map<String, i64>, i64>) -> i64 { return 1; }";
    let reason = rejected(src, "f", &["f"]);
    assert!(reason.contains("parameter `m`"), "{reason}");
    assert!(reason.contains("`Map<Map<String, i64>, i64>`"), "{reason}");
}

// r6. two bindings of one name are rejected by NAME: LIR's flat scopes are
// the reason, and the message has to say so or it reads like a bug.
//
// Source can no longer produce this state —
// the lowering α-renames the second binding, and t01 checks that it does —
// so the collision is introduced here by UNDOING the rename. The guard is a
// backstop against a lowering that forgets to rename, and this is what such
// a lowering would hand the walker.
#[test]
fn r6_duplicate_binding_reason_names_the_binding() {
    let src = "fn f() { let mut i = 0; while i < 2 { let x = i; print(x); i = i + 1; } \
                   let mut j = 0; while j < 2 { let x = j; print(x); j = j + 1; } }";
    let (mut f, tables) = lir_fn_and_tables(src, "f", &["f"]);
    let mut undone = false;
    for block in &mut f.blocks {
        for inst in &mut block.instrs {
            if let LirInst::Let { name, .. } = inst
                && let Some(source) = name.split_once('$').map(|(base, _)| base.to_string())
            {
                *name = source;
                undone = true;
            }
        }
    }
    for local in &mut f.locals {
        if let Some((base, _)) = local.name.split_once('$') {
            local.name = base.to_string();
        }
    }
    assert!(undone, "the lowering did not rename the second `let x`");
    let reason = tables
        .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
        .expect("a duplicated binding is rejected");
    assert!(
        reason.starts_with("LIR local `x` reuses an existing lowered name"),
        "{reason}"
    );
}

// r7. a `let` whose BINDING type is unsupported blames the binding, not the
// initialiser (the slot's type is what the walker cannot represent).
#[test]
fn r7_let_binding_type_reason() {
    let src = "import std::collections::Map; \
                   fn f() { let m: Map<Map<String, i64>, i64> = Map::new(); print(1); }";
    let reason = rejected(src, "f", &["f"]);
    assert!(
        reason.starts_with("`let m` binds type `Map<Map<String, i64>, i64>`"),
        "{reason}"
    );
}

// r8. the blamed node is the SMALLEST unsupported one: an unknown callee
// nested three levels down is named, not the enclosing `let`.
#[test]
fn r8_reason_blames_the_innermost_node() {
    // `g` exists for the type checker but is NOT in the walker's known
    // symbols, which is what makes the call the unsupported node.
    let src = "fn g() -> i64 { println(0); return 2; } fn f() -> i64 { let a = 1 + (2 * g()); return a; }";
    let reason = rejected(src, "f", &["f"]);
    assert!(reason.starts_with("the call to `g` at line"), "{reason}");
}

// r9. the line is the offending node's, not the function's or the
// statement's — the whole point is to be able to jump to it.
#[test]
fn r9_reason_reports_the_node_line() {
    let src = "fn g() -> i64 { println(0); return 2; }\n\
                   fn f() -> i64 {\n\
                       let a = 1;\n\
                       let b = a + g();\n\
                       return b;\n\
                   }";
    let reason = rejected(src, "f", &["f"]);
    assert!(reason.contains("at line 4"), "{reason}");
}

// r10. a node whose own type is outside the subset says so; that is a
// different fix from an unsupported construct with a fine type.
#[test]
fn r10_unsupported_type_is_reported_with_the_node() {
    // The argument is the unsupported node: the outer call is `i64` and
    // its callee is known, so only the `make()` inside it can be at fault.
    let src = "import std::collections::Map; \
                   fn make() -> Map<Map<String, i64>, i64> { return Map::new(); } \
                   fn take(m: Map<Map<String, i64>, i64>) -> i64 { return 1; } \
                   fn f() -> i64 { return take(make()); }";
    let reason = rejected(src, "f", &["f", "take", "make"]);
    assert!(reason.starts_with("the call to `make` at line"), "{reason}");
    assert!(
        reason.contains("has type `Map<Map<String, i64>, i64>`"),
        "{reason}"
    );
}

// r11. a straight-line defer scope is admitted by the LIR walker.
#[test]
fn r11_straight_line_defer_is_eligible() {
    let src = "fn f() { defer { print(2); } print(1); }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// r11b. An explicit return lowers the scope exit to `FlushDefers` in a
// different block. The dataflow eligibility check must carry the open
// scope to that block and treat the flush as closing it on that path.
#[test]
fn r11b_cross_block_defer_with_early_return_is_eligible() {
    let src = "fn f(n: i64) -> i64 { defer print(n); if n > 0 { return 1; } return 0; }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// r11d. `break` leaves a loop body's defer scope with a `FlushDefers` and no
// matching `LeaveDeferScope`, and the block it jumps to is also reached from
// the loop header with the scope closed. Both edges agree only once the
// flush is read as closing the scope on its own path.
#[test]
fn r11d_break_out_of_a_defer_scope_is_eligible() {
    let src = "fn f(n: i64) -> i64 {
    let mut t = 0;
    for i in 0..n { defer print(i); if i == 2 { break; } t = t + i; }
    return t;
}";
    assert!(eligible_checked(src, "f", &["f"]));
}

// r11e. `continue` is the same exit against the loop latch, which the LIR
// emits BEFORE the body that jumps to it.
#[test]
fn r11e_continue_out_of_a_defer_scope_is_eligible() {
    let src = "fn f(n: i64) -> i64 {
    let mut t = 0;
    for i in 0..n { defer print(i); if i == 1 { continue; } t = t + i; }
    return t;
}";
    assert!(eligible_checked(src, "f", &["f"]));
}

// r11f. `defer` combined with `?` was rejected outright, because the
// walker's propagating exit had no unwinding. It emits one now.
#[test]
fn r11f_defer_combined_with_try_is_eligible() {
    let src = "fn g(n: i64) -> Result<i64, String> { return Ok(n); }
fn f(n: i64) -> Result<i64, String> {
    defer print(n);
    let v = g(n)?;
    return Ok(v);
}";
    assert!(eligible_checked(src, "f", &["f", "g"]));
}

// r11g. Three scopes deep with the exit in the innermost: the flush names
// every open scope at once, and the block that closes the middle one is
// emitted before the block that closes the inner one.
#[test]
fn r11g_three_defer_scopes_with_an_inner_return_are_eligible() {
    let src = "fn f(n: i64) -> i64 {
    defer print(0);
    for i in 0..n {
        defer print(i);
        if i > 0 { defer print(9); if i == 2 { return i; } }
    }
    return -1;
}";
    assert!(eligible_checked(src, "f", &["f"]));
}

// r11h. A scope opened on only ONE side of a branch leaves the two edges
// into the join disagreeing about what is open. No source program lowers to
// this — `defer` is lexical — so it is built by hand, and it is what the
// agreement check rejects. The blocks
// are all reachable, so this is the disagreement and not the separate
// unreachable-block rejection.
#[test]
fn r11h_unbalanced_defer_scopes_across_a_join_are_rejected() {
    let block = |id: usize, instrs: Vec<LirInst>, terminator: Terminator| LirBlock {
        id: BlockId(id),
        instrs,
        terminator,
        recovery: Vec::new(),
    };
    // Siteless, so the balanced graph needs no registration body: only the
    // scope's openness differs between the two edges.
    let enter = LirInst::EnterDeferScope {
        sites: Vec::new(),
        resume: None,
        lock: None,
    };
    let f = LirFunction {
        name: "f".to_string().into(),
        params: Vec::new(),
        return_type: Type::Void,
        is_async: false,
        locals: Vec::new(),
        captures: Vec::new(),
        async_frame: Default::default(),
        blocks: vec![
            block(
                0,
                Vec::new(),
                Terminator::Branch {
                    cond: crate::ir::lowered::LirOperand::Bool(true),
                    then_block: BlockId(1),
                    else_block: BlockId(2),
                },
            ),
            block(1, vec![enter], Terminator::Jump(BlockId(3))),
            block(2, Vec::new(), Terminator::Jump(BlockId(3))),
            block(3, Vec::new(), Terminator::Return(None)),
        ],
    };
    assert!(!lir_cleanup_graph_is_valid(&f));

    // The same graph with the scope opened before the branch instead — the
    // edges agree, and it is accepted.
    let mut balanced = f;
    balanced.blocks[0].instrs = std::mem::take(&mut balanced.blocks[1].instrs);
    assert!(lir_cleanup_graph_is_valid(&balanced));
}

// r11c. A synchronous recover-capable scope needs an explicit LIR resume
// target even when its normal source path returns before the scope ends.
// Without it Cranelift sees a branch to an empty backend-only block.
#[test]
fn r11c_sync_recovery_records_a_lexical_continuation() {
    let src = r#"
fn f() {
    if true {
        defer match recover() {
            Some(info) => println("return:" + info.message),
            None => {}
        }
        defer { panic("cleanup"); }
        return;
    }
    println("resumed");
}
"#;
    let (f, _tables) = lir_fn_and_tables(src, "f", &["f"]);
    assert!(f.blocks.iter().any(|block| {
        block.instrs.iter().any(|inst| {
            matches!(
                inst,
                LirInst::EnterDeferScope {
                    resume: Some(_),
                    ..
                }
            )
        })
    }));
}

// r12. Source-declared static stores are eligible since.
// Removing the registered storage models a broken declaration pass and
// proves the diagnostic names the exact class and field.
#[test]
fn r12_unresolved_static_field_store_reason() {
    let src = "class Counter { pub static mut total: i64 = 0; \
                   pub static fn bump() { Counter::total = Counter::total + 1; } }";
    let (f, mut tables) = lir_fn_and_tables(src, "Counter::bump", &[]);
    tables
        .static_fields
        .remove(&("Counter".to_string(), "total".to_string()));
    let reason = tables
        .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
        .expect("missing static storage must reject the function");
    assert!(reason.contains("Counter::total"), "{reason}");
}

// A lowered super constructor still requires its registered class layout.
// Corrupt backend metadata after valid source lowering to exercise that gate.
#[test]
fn r13_super_init_without_registered_base_layout_is_rejected() {
    let source = "open class Base { pub init(self) {} } class Child extends Base { pub init(self) { super.init(); } }";
    let (function, mut tables) = lir_fn_and_tables(source, "Child::init", &[]);
    assert!(tables.with_ctx(|ctx| lir_supported_function(&function, ctx)));
    let retained: Vec<_> = tables
        .class_layouts
        .iter()
        .filter(|(id, _)| **id != TypeId::local("Base"))
        .map(|(id, layout)| (*id, layout.clone()))
        .collect();
    tables.class_layouts.clear();
    for (id, layout) in retained {
        tables.class_layouts.insert(id, layout);
    }
    assert!(
        tables
            .with_ctx(|ctx| lir_rejection_reason(&function, ctx))
            .is_some(),
        "an unregistered constructor layout must reject"
    );
}

// r14. a `return` of a `void` value has no slot in the signature; the
// message says that rather than blaming the expression.
#[test]
fn r14_void_return_value_reason() {
    let src = "fn g() {} fn f() { return g(); }";
    let reason = rejected(src, "f", &["f", "g"]);
    assert!(reason.contains("yields a `void` value"), "{reason}");
}

// r15. when a lambda IS the blocker — here because its lifted symbol was
// never registered, the state an imported module still produces — it is
// reported as a lambda and NOT descended into: its body binds its own
// parameters, so a name inside would be blamed as unbound.
#[test]
fn r15_lambda_is_not_descended_into() {
    let src = "fn apply(g: fn(i64) -> i64) -> i64 { return g(2); } \
                   fn caller() -> i64 { return apply(|x: i64| -> i64 { return x + 1; }); }";
    let fns = &["caller", "apply"];
    let (f, mut tables) = lir_fn_and_tables(src, "caller", fns);
    tables.lambdas.clear();
    let reason = tables
        .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
        .expect("an unregistered lambda must refuse");
    assert!(reason.contains("a lambda"), "{reason}");
    assert!(!reason.contains("the variable `x`"), "{reason}");
}

// r16. an arm whose pattern the walker cannot bind is skipped for the same
// reason: its body's bindings are not in `names`, so the `match` itself is
// the honest answer.
#[test]
fn r16_unbindable_arm_blames_the_match() {
    // A `ClassDowncast` binds only when the walker can represent the class
    // it binds. `Sq` stores a map whose KEY is a reference the runtime
    // cannot read (see c15b), so it cannot.
    let src = "import std::collections::Map; \
                   interface Shape { fn area(self) -> i64; } \
                   class Sq implements Shape { \
                       pub b: Map<Map<String, i64>, i64>; \
                       pub fn area(self) -> i64 { return 1; } \
                   } \
                   fn pick(s: Shape) -> i64 { return match s { Sq(q) => 1, _ => 0 }; }";
    let reason = rejected(src, "pick", &["pick"]);
    assert!(
        reason.starts_with("the `match` arm") && reason.contains("pattern outside"),
        "{reason}"
    );
}

// r17. when the scrutinee is the problem, the scrutinee is blamed — the
// arms may all be perfectly supported.
#[test]
fn r17_match_blames_its_scrutinee() {
    let src = "enum Color { Red, Green } \
                   fn g() -> Color { return Color::Red; } \
                   fn f() -> i64 { return match g() { Color::Red => 1, _ => 0 }; }";
    let reason = rejected(src, "f", &["f"]);
    assert!(reason.starts_with("the call to `g`"), "{reason}");
}

// r18. inside a bindable arm the search continues, so a bad expression in
// an arm BODY is what gets named.
#[test]
fn r18_arm_body_expression_is_blamed() {
    let src = "enum Color { Red, Green } \
                   fn g() -> i64 { println(0); return 1; } \
                   fn f(c: Color) -> i64 { return match c { Color::Red => g(), _ => 0 }; }";
    let reason = rejected(src, "f", &["f"]);
    assert!(reason.starts_with("the call to `g`"), "{reason}");
}

// r19. an arm binding is IN scope while its body is searched: blaming the
// pattern's own variable would be the classic false positive here.
#[test]
fn r19_arm_binding_is_in_scope_for_the_body() {
    let src = "enum Shape { Circle(i64) } \
                   fn g() -> i64 { println(0); return 1; } \
                   fn f(s: Shape) -> i64 { return match s { Shape::Circle(r) => r + g() }; }";
    let reason = rejected(src, "f", &["f"]);
    assert!(!reason.contains("the variable `r`"), "{reason}");
    assert!(reason.starts_with("the call to `g`"), "{reason}");
}

// r20. with several blockers the FIRST in program order wins, so a
// whole-corpus histogram of reasons is stable between runs.
#[test]
fn r20_first_blocker_in_program_order_wins() {
    let src = "fn g() -> i64 { println(0); return 1; }\n\
                   fn h() -> i64 { println(0); return 2; }\n\
                   fn f() -> i64 {\n\
                       let a = g();\n\
                       let b = h();\n\
                       return a + b;\n\
                   }";
    let reason = rejected(src, "f", &["f"]);
    assert!(reason.starts_with("the call to `g` at line 4"), "{reason}");
}

// r21. a method call names both the method and the receiver's type: the
// same method name on two receivers is two different gaps.
#[test]
fn r21_method_call_names_method_and_receiver() {
    // Scalar `toString` joined the subset in, so the shape
    // that still gets refused is the one k17 covers: an interface method
    // the backend's tables do not register a slot for.
    let src = format!("{NAMED} fn f(n: Named) -> String {{ return n.name(); }}");
    let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
    tables.iface_methods.clear();
    let reason = tables
        .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
        .expect("`f` was accepted");
    assert!(
        reason.contains("the method `name` on a `Named`"),
        "{reason}"
    );
}

// r22. A declared static read is eligible. Removing its registration proves
// the rejection still names the class and field, matching store r12.
#[test]
fn r22_unresolved_static_property_read_is_named() {
    let src = "class Config { pub static version: i64 = 1; } \
                   fn f() -> i64 { return Config::version; }";
    let (f, mut tables) = lir_fn_and_tables(src, "f", &["f"]);
    tables
        .static_fields
        .remove(&("Config".to_string(), "version".to_string()));
    let reason = tables
        .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
        .expect("missing static storage must reject the read");
    assert!(
        reason.contains("the static property `Config::version`"),
        "{reason}"
    );
}

// r23. an operator is named by its source spelling, shared with the IR
// dumper so one operator is never described two ways.
#[test]
fn r23_operator_named_by_source_spelling() {
    let src = "import std::collections::Map; \
                   fn f() -> i64 { let a: Map<Map<String, i64>, i64> = Map::new(); let b = 1 + 2; return b; }";
    // `**` is the interesting spelling; check the shared table directly so
    // this does not depend on which operators the walker currently takes.
    assert_eq!(binop_str(&BinOp::Pow), "**");
    assert!(reason_of(src, "f", &["f"]).is_some());
}

// r24. `new C` on a class outside the subset is named with the class, so a
// whole-file scan groups by the class that needs the work.
#[test]
fn r24_new_names_its_class() {
    // In argument position, so the `let`'s binding type is not what gets
    // blamed first. `Dog` is outside the subset because of the
    // array-keyed map it stores (see c15b) — taking part in inheritance no
    // longer puts a class out.
    let src = "import std::collections::Map; \
                   class Dog { pub m: Map<Map<String, i64>, i64>; \
                       pub init(self) { self.m = Map::new(); } } \
                   fn show(d: Dog) { println(1); } \
                   fn caller() { show(new Dog()); }";
    let reason = rejected(src, "caller", &["caller", "show"]);
    assert!(reason.starts_with("`new Dog` at line"), "{reason}");
}

// ──: function values, lambdas and indirect calls ────────
//
// Three HIR shapes arrived together and only make sense together: a named
// function used as a VALUE (`FnRef`), a lambda expression (a lifted
// top-level function with no captured environment — the checker rejects a
// capture outright, E1002), and a call whose callee is a local function
// value rather than a symbol. The `f*` tests pin the eligibility boundary;
// the OUTPUT is pinned by the `lir_diff_*` differentials in
// tests/integration/codegen.rs.

/// Whether the walker admits each lifted lambda in `src`, in lowering
/// order. A lambda is a function in its own right: its body is vetted under
/// its own symbol, not as part of whoever takes its address.
fn lambda_eligibility(src: &str, fns: &[&str]) -> Vec<bool> {
    let (p, tables) = checked_lowering(src, fns);
    p.lambdas
        .iter()
        .map(|l| tables.with_ctx(|ctx| lir_supported_function(&l.function, ctx)))
        .collect()
}

// f01. the base case: a named function used as a value. Spelled as a bare
// identifier, so what makes it a function address rather than a variable
// read is purely what the name resolves to.
#[test]
fn f01_named_function_as_a_value_eligible() {
    let src = "fn double(x: i64) -> i64 { return x * 2; }
                   fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(double, 10); }";
    let fns = &["double", "apply", "caller"];
    for name in fns {
        assert!(eligible_checked(src, name, fns), "`{name}` must be in");
    }
}

// f02. a `fn(...)` PARAMETER is a supported type — the walker has to accept
// the type before it can accept the call through it.
#[test]
fn f02_fn_typed_parameter_is_a_supported_type() {
    let src = "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }";
    assert!(eligible_checked(src, "apply", &["apply"]));
}

// f03. a `fn`-typed `let`, then a call through it. The binding is an
// ordinary local slot holding a code address.
#[test]
fn f03_fn_typed_let_and_indirect_call() {
    let src = "fn triple(x: i64) -> i64 { return x * 3; }
                   fn caller() -> i64 { let g: fn(i64) -> i64 = triple; return g(7); }";
    assert!(eligible_checked(src, "caller", &["triple", "caller"]));
}

// f04. a lambda expression is a value like any other, and the lifted body
// is a function the walker compiles on its own terms.
#[test]
fn f04_lambda_value_and_its_lifted_body_are_both_eligible() {
    let src = "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(|x: i64| x + 1, 10); }";
    assert!(eligible_checked(src, "caller", &["apply", "caller"]));
    assert_eq!(lambda_eligibility(src, &["apply", "caller"]), vec![true]);
}

// f05. the two are INDEPENDENT: a lambda whose body is outside the subset
// costs only the lambda its LIR compilation. Taking its address is a
// relocation, so the enclosing function does not care what it contains.
#[test]
fn f05_unsupported_lambda_body_does_not_sink_its_taker() {
    // The lifted body calls a symbol the backend never declared, so it
    // fails validation independently. (`format` used to serve as the
    // unsupported body here; it joined the subset in.)
    let src = "fn helper(x: i64) -> String { return \"h\"; }
                   fn apply(f: fn(i64) -> String, v: i64) -> String { return f(v); }
                   fn caller() -> String { return apply(|x: i64| helper(x), 1); }";
    let fns = &["apply", "caller"];
    assert_eq!(lambda_eligibility(src, fns), vec![false]);
    assert!(eligible_checked(src, "caller", fns));
}

// f06. a lambda nested inside a lambda is lifted too — the walk goes
// through `HirExpr::children`, innermost first, so both bodies exist as
// functions and neither is left inline in the other's block graph.
#[test]
fn f06_nested_lambdas_are_both_lifted() {
    let src = "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(|x: i64| apply(|y: i64| y * 2, x), 5); }";
    let fns = &["apply", "caller"];
    assert_eq!(lambda_eligibility(src, fns), vec![true, true]);
    assert!(eligible_checked(src, "caller", fns));
}

// f07. the walker never takes the address of a symbol it cannot name. The
// lambda's SYMBOL comes from the backend's ID-keyed table, not from the
// IR, so an unregistered lambda (one inside an imported module, today)
// must refuse rather than emit an address of nothing.
#[test]
fn f07_unregistered_lambda_symbol_refuses() {
    let src = "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(|x: i64| x + 1, 10); }";
    let fns = &["apply", "caller"];
    let (f, mut tables) = lir_fn_and_tables(src, "caller", fns);
    assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    tables.lambdas.clear();
    assert!(tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)));
}

// f08. the same rule for a NAMED function: the address is only taken when
// the function is one the backend declared. A name the compiler knows a
// type for but never declared would relocate against nothing.
#[test]
fn f08_unknown_function_value_refuses() {
    let src = "fn double(x: i64) -> i64 { return x * 2; }
                   fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(double, 10); }";
    let fns = &["double", "apply", "caller"];
    let (f, mut tables) = lir_fn_and_tables(src, "caller", fns);
    assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    tables.known.remove("double");
    assert!(tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)));
}

// f09. a function whose parameters are not all by-value has no honest
// function-pointer value: reference parameters use a
// different ABI, so its address must not be handed to an indirect call.
#[test]
fn f09_by_reference_parameters_are_not_function_values() {
    let src = "fn double(x: i64) -> i64 { return x * 2; }
                   fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(double, 10); }";
    let fns = &["double", "apply", "caller"];
    let (f, mut tables) = lir_fn_and_tables(src, "caller", fns);
    tables.param_modes.insert(
        "double",
        vec![ParamMode::Reference {
            mutable: false,
            ampersand_span: crate::diagnostics::Span::dummy(),
            mut_span: None,
        }],
    );
    assert!(tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)));
}

// f10. the function TYPE is vetted structurally: every parameter and the
// return must themselves be supported, and a `void` parameter — which has
// no ABI slot — is excluded explicitly.
#[test]
fn f10_function_type_is_vetted_structurally() {
    let (_, tables) = checked_lowering("fn f() {}", &["f"]);
    tables.with_ctx(|ctx| {
        let ok = Type::Fn(vec![Type::I64, Type::String], Box::new(Type::Bool));
        assert!(ctx.supported_type(&ok));
        // `void` in a parameter position has no slot to pass.
        let void_param = Type::Fn(vec![Type::Void], Box::new(Type::I64));
        assert!(!ctx.supported_type(&void_param));
        // a `void` RETURN is fine — that is an ordinary statement call.
        let void_ret = Type::Fn(vec![Type::I64], Box::new(Type::Void));
        assert!(ctx.supported_type(&void_ret));
        // an unsupported component sinks the whole type.
        let bad = Type::Fn(
            vec![Type::Named("Missing".to_string().into())],
            Box::new(Type::I64),
        );
        assert!(!ctx.supported_type(&bad));
    });
}

// f11. the value's type must be the one the walker would emit. `fn_value_of`
// answers from the registered signature; if the expression's recorded type
// disagrees, the address would be called through the wrong signature.
#[test]
fn f11_function_value_type_must_match_the_registration() {
    let src = "fn double(x: i64) -> i64 { return x * 2; }
                   fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(double, 10); }";
    let fns = &["double", "apply", "caller"];
    let (f, mut tables) = lir_fn_and_tables(src, "caller", fns);
    tables
        .fn_types
        .insert("double", Type::Fn(vec![Type::String], Box::new(Type::I64)));
    assert!(tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)));
}

// f12. a local function value SHADOWS a top-level function of the same
// name, and eligibility resolves the callee the same way the emitter does —
// local first. Getting this order wrong would type-check one callee and
// call the other.
#[test]
fn f12_local_function_value_shadows_a_top_level_name() {
    let src = "fn weigh(n: i64) -> i64 { return n * 3; }
                   fn caller() -> i64 { let weigh: fn(i64) -> i64 = |n: i64| n; return weigh(2); }";
    let fns = &["weigh", "caller"];
    assert!(eligible_checked(src, "caller", fns));
}

// f13. a `void`-returning function value called in statement position: the
// indirect call has no result to merge, which is a different signature and
// a different emission path from the valued one.
#[test]
fn f13_void_returning_function_value() {
    let src = "fn shout(n: i64) { println(n); }
                   fn run(f: fn(i64) -> void) { f(1); }
                   fn caller() { run(shout); }";
    let fns = &["shout", "run", "caller"];
    for name in fns {
        assert!(eligible_checked(src, name, fns), "`{name}` must be in");
    }
}

// f14. an indirect call's ARGUMENTS are vetted against the function type's
// parameters, not against a symbol's signature — there is no symbol.
#[test]
fn f14_indirect_call_arguments_are_vetted_against_the_fn_type() {
    let src = "fn join(a: String, b: i64) -> String { return a + b.toString(); }
                   fn caller() -> String {
                       let g: fn(String, i64) -> String = join;
                       return g(\"n=\", 2);
                   }";
    assert!(eligible_checked(src, "caller", &["join", "caller"]));
}

// f15. an indirect call's RESULT must have the expression's type. Source
// cannot produce a mismatch, so the recorded type is perturbed directly —
// the state a desugaring bug would leave behind.
#[test]
fn f15_indirect_call_result_type_is_checked() {
    let src = "fn caller(g: fn(i64) -> i64) -> i64 { return g(7); }";
    let (mut f, tables) = lir_fn_and_tables(src, "caller", &["caller"]);
    assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    let Terminator::Return(Some(call)) = &mut f.blocks[0].terminator else {
        panic!(
            "expected a returned indirect call, got {:?}",
            f.blocks[0].terminator
        );
    };
    let crate::ir::lowered::LirOperand::Local(result) = call else {
        panic!("materialized indirect call result");
    };
    f.locals[result.0 as usize].ty = Type::String;
    assert!(tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)));
}

// f16. function values flow through the ordinary type positions: a
// parameter, a `let`, a return type, and an array element.
#[test]
fn f16_function_values_in_every_type_position() {
    let src = "import std::collections::Array;
                   fn double(x: i64) -> i64 { return x * 2; }
                   fn pick() -> fn(i64) -> i64 { return double; }
                   fn table() -> i64 {
                       let fs: Array<fn(i64) -> i64> = [double];
                       let g = fs[0];
                       return g(4);
                   }";
    let fns = &["double", "pick", "table"];
    for name in fns {
        assert!(eligible_checked(src, name, fns), "`{name}` must be in");
    }
}

// f17. a GC-managed payload crossing an indirect call: the argument is
// rooted before the call exactly as a direct call's is, so a collection
// inside the callee cannot lose it.
#[test]
fn f17_gc_managed_arguments_cross_an_indirect_call() {
    let src = "fn shout(s: String) -> String { return s + \"!\"; }
                   fn run(f: fn(String) -> String, s: String) -> String { return f(s); }
                   fn caller() -> String { return run(shout, \"hi\"); }";
    let fns = &["shout", "run", "caller"];
    for name in fns {
        assert!(eligible_checked(src, name, fns), "`{name}` must be in");
    }
}

// f18. a lambda inside a CLASS METHOD is lifted like any other — the
// collection walks methods, not only free functions.
#[test]
fn f18_lambda_inside_a_class_method_is_lifted() {
    let src = "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   class Box2 {
                       pub v: i64;
                       pub fn scaled(self) -> i64 { return apply(|x: i64| x * 2, self.v); }
                   }";
    assert_eq!(lambda_eligibility(src, &["apply"]), vec![true]);
}

// f19. `Option::map` — the first combinator that CALLS its operand. The
// result is `Option<U>` for the callable's return `U`, which is what
// decides the new value's representation.
#[test]
fn f19_option_map_result_type_follows_the_callable() {
    let opt_i64 = Type::Generic("Option".to_string().into(), vec![Type::I64]);
    let to_string = Type::Fn(vec![Type::I64], Box::new(Type::String));
    assert_eq!(
        option_result_method(&opt_i64, "map", &[to_string]),
        Some(Type::Generic(
            "Option".to_string().into(),
            vec![Type::String]
        ))
    );
    // a `void`-returning callable would build a `Some` with no payload slot
    let to_void = Type::Fn(vec![Type::I64], Box::new(Type::Void));
    assert_eq!(option_result_method(&opt_i64, "map", &[to_void]), None);
    // the callable's parameter must be the payload type
    let wrong = Type::Fn(vec![Type::String], Box::new(Type::I64));
    assert_eq!(option_result_method(&opt_i64, "map", &[wrong]), None);
}

// f20. `and_then` and `or_else` MERGE one arm's receiver with the other
// arm's callable result, so the payload types have to line up — but the
// error type of a `Result` deliberately does not: a lambda ending in
// `Result::Ok(0)` records `Result<i64, void>`, and every `Result` is the
// same two-word box whatever its type arguments.
#[test]
fn f20_and_then_and_or_else_merge_rules() {
    let opt_i64 = Type::Generic("Option".to_string().into(), vec![Type::I64]);
    let opt_str = Type::Generic("Option".to_string().into(), vec![Type::String]);
    let to_opt_str = Type::Fn(vec![Type::I64], Box::new(opt_str.clone()));
    assert_eq!(
        option_result_method(&opt_i64, "and_then", &[to_opt_str]),
        Some(opt_str.clone())
    );
    // `or_else` takes NO argument and must produce the receiver's payload
    let same = Type::Fn(vec![], Box::new(opt_i64.clone()));
    assert_eq!(
        option_result_method(&opt_i64, "or_else", &[same]),
        Some(opt_i64.clone())
    );
    let other = Type::Fn(vec![], Box::new(opt_str));
    assert_eq!(option_result_method(&opt_i64, "or_else", &[other]), None);

    let res = Type::Generic("Result".to_string().into(), vec![Type::I64, Type::String]);
    let unresolved_err = Type::Generic("Result".to_string().into(), vec![Type::I64, Type::Void]);
    let recover = Type::Fn(vec![Type::String], Box::new(unresolved_err.clone()));
    assert_eq!(
        option_result_method(&res, "or_else", &[recover]),
        Some(unresolved_err)
    );
    // the OK payload still has to match — that arm passes the receiver on
    let mismatched = Type::Fn(
        vec![Type::String],
        Box::new(Type::Generic(
            "Result".to_string().into(),
            vec![Type::String, Type::String],
        )),
    );
    assert_eq!(option_result_method(&res, "or_else", &[mismatched]), None);
}

// f21. `map_err` is a `Result`-only combinator, and it rebuilds the ERROR
// side while passing the ok payload through.
#[test]
fn f21_map_err_is_result_only() {
    let res = Type::Generic("Result".to_string().into(), vec![Type::I64, Type::String]);
    let wrap = Type::Fn(vec![Type::String], Box::new(Type::String));
    assert_eq!(
        option_result_method(&res, "map_err", std::slice::from_ref(&wrap)),
        Some(res.clone())
    );
    let opt = Type::Generic("Option".to_string().into(), vec![Type::I64]);
    assert_eq!(option_result_method(&opt, "map_err", &[wrap]), None);
}

// f22. a `void` payload has no slot for a combinator to read or write, so
// `Result<void, E>` is excluded from all of them — the same rule the
// unwrap family already followed.
#[test]
fn f22_void_payloads_are_excluded_from_combinators() {
    let res_void = Type::Generic("Result".to_string().into(), vec![Type::Void, Type::String]);
    let f = Type::Fn(vec![Type::Void], Box::new(Type::I64));
    for method in ["map", "and_then"] {
        assert_eq!(
            option_result_method(&res_void, method, std::slice::from_ref(&f)),
            None,
            "`{method}` must not claim a void payload"
        );
    }
}

// f23. the combinators are still reachable end to end from source, with a
// lambda operand and with a named function value — the two spellings the
// emitter has to accept.
#[test]
fn f23_combinators_from_source_with_both_operand_spellings() {
    let src = "fn twice(v: i64) -> i64 { return v * 2; }
                   fn with_lambda(x: Option<i64>) -> i64 { return x.map(|v: i64| v * 2).unwrap_or(0); }
                   fn with_fn_value(x: Option<i64>) -> i64 { return x.map(twice).unwrap_or(0); }";
    let fns = &["twice", "with_lambda", "with_fn_value"];
    for name in fns {
        assert!(eligible_checked(src, name, fns), "`{name}` must be in");
    }
}

// f24. recursion THROUGH a function value: the callee is a parameter, so
// the call graph is not statically known and the panic-depth protocol has
// to stay conservative. Eligibility must still admit it.
#[test]
fn f24_recursion_through_a_function_value() {
    let src = "fn step(n: i64) -> i64 { return n - 1; }
                   fn walk(f: fn(i64) -> i64, n: i64) -> i64 {
                       if n <= 0 { return 0; }
                       return 1 + walk(f, f(n));
                   }
                   fn caller() -> i64 { return walk(step, 3); }";
    let fns = &["step", "walk", "caller"];
    for name in fns {
        assert!(eligible_checked(src, name, fns), "`{name}` must be in");
    }
}

// ---------------------------------------------------------------------
// d. divergence, scalar `toString` and `format`.
//
// Divergence is a property of the POSITION, not of the expression: a
// `!`-typed expression ends its Cranelift block with a terminator, so the
// walker admits one only where nothing follows it in the same block — as a
// whole statement, or as the tail of a `match` arm. Everything else here
// is the string machinery those panics need to build their messages.
// ---------------------------------------------------------------------

// d01. all four scalar receivers convert. `toString` is the only builtin
// the walker resolves by intrinsic rather than by name, so each arm of that
// table needs its own witness.
#[test]
fn d01_every_scalar_to_string_is_eligible() {
    let cases = [
        ("i64", "n: i64", "n"),
        ("f64", "n: f64", "n"),
        ("bool", "n: bool", "n"),
        ("String", "n: String", "n"),
    ];
    for (label, param, recv) in cases {
        let src = format!("fn f({param}) -> String {{ return {recv}.toString(); }}");
        assert!(eligible_checked(&src, "f", &["f"]), "{label} must convert");
    }
}

// d02. `String::toString` is the identity, not a no-op the resolver drops:
// it must still resolve, and to `String`.
#[test]
fn d02_string_to_string_is_the_identity() {
    assert_eq!(
        scalar_to_string(&Type::String, "toString", &[]),
        Some(Type::String)
    );
}

// d03. arity is part of the match. A one-argument `toString` is not the
// intrinsic, and admitting it would emit a call with a stranded operand.
#[test]
fn d03_scalar_to_string_is_arity_checked() {
    let arg = HirExpr {
        kind: HirExprKind::Int(1),
        ty: Type::I64,
        span: crate::diagnostics::Span::dummy(),
    };
    assert_eq!(
        scalar_to_string(&Type::I64, "toString", std::slice::from_ref(&arg)),
        None
    );
}

// d04. a non-scalar receiver never reaches the scalar table — collections
// and class receivers have their own lowerings, and silently borrowing this
// one would emit the wrong runtime symbol.
#[test]
fn d04_non_scalar_receivers_are_not_scalar_to_string() {
    for recv in [
        Type::Array(Box::new(Type::I64)),
        Type::Named("Widget".to_string().into()),
        Type::Void,
    ] {
        assert_eq!(scalar_to_string(&recv, "toString", &[]), None, "{recv:?}");
    }
}

// d05. `format` renders exactly the four scalar operand types, mixed
// freely with literal text.
#[test]
fn d05_format_renders_every_scalar_operand() {
    let src = "fn f(a: i64, b: f64, c: bool, d: String) -> String {
                       return format(\"{} {} {} {}\", a, b, c, d);
                   }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// d06. a precision placeholder passes its operand straight to an f64
// formatting symbol, so an i64 there would reinterpret the bits.
#[test]
fn d06_precision_placeholder_demands_f64() {
    let ok = "fn f(x: f64) -> String { return format(\"{:.6f}\", x); }";
    assert!(eligible_checked(ok, "f", &["f"]));

    let span = crate::diagnostics::Span::dummy();
    let spec = HirExpr {
        kind: HirExprKind::Str("{:.6f}".to_string()),
        ty: Type::String,
        span,
    };
    let int_operand = HirExpr {
        kind: HirExprKind::Int(1),
        ty: Type::I64,
        span,
    };
    assert_eq!(format_operands(&[spec, int_operand]), None);
}

// d07. placeholder count and operand count must agree in BOTH directions:
// too few operands reads past the argument list, too many silently drops
// an evaluated value.
#[test]
fn d07_format_arity_must_match_in_both_directions() {
    let span = crate::diagnostics::Span::dummy();
    let operand = |ty: Type| HirExpr {
        kind: HirExprKind::Int(1),
        ty,
        span,
    };
    let spec = |text: &str| HirExpr {
        kind: HirExprKind::Str(text.to_string()),
        ty: Type::String,
        span,
    };
    assert_eq!(format_operands(&[spec("{} {}"), operand(Type::I64)]), None);
    assert_eq!(
        format_operands(&[spec("{}"), operand(Type::I64), operand(Type::I64)]),
        None
    );
    assert_eq!(
        format_operands(&[spec("{}"), operand(Type::I64)]).map(<[HirExpr]>::len),
        Some(1)
    );
}

// d08. `{{` and `}}` are literal braces, not placeholders. Counting them
// as placeholders would make a correct call look arity-mismatched.
#[test]
fn d08_escaped_braces_are_literals_not_placeholders() {
    let span = crate::diagnostics::Span::dummy();
    let spec = HirExpr {
        kind: HirExprKind::Str("{{literal}} {}".to_string()),
        ty: Type::String,
        span,
    };
    let operand = HirExpr {
        kind: HirExprKind::Int(9),
        ty: Type::I64,
        span,
    };
    assert_eq!(
        format_operands(&[spec, operand]).map(<[HirExpr]>::len),
        Some(1)
    );
}

// d09. the spec must be a literal. A computed spec cannot be parsed at
// compile time, so the walker cannot know what to emit.
#[test]
fn d09_format_spec_must_be_a_literal() {
    let span = crate::diagnostics::Span::dummy();
    let computed = HirExpr {
        kind: HirExprKind::Var("s".to_string()),
        ty: Type::String,
        span,
    };
    assert_eq!(format_operands(std::slice::from_ref(&computed)), None);
}

// d10. an operand the runtime has no renderer for is refused even though
// the arity agrees — the check is per operand, not just a count.
#[test]
fn d10_unrenderable_format_operand_is_refused() {
    let span = crate::diagnostics::Span::dummy();
    let spec = HirExpr {
        kind: HirExprKind::Str("{}".to_string()),
        ty: Type::String,
        span,
    };
    let array = HirExpr {
        kind: HirExprKind::Var("a".to_string()),
        ty: Type::Array(Box::new(Type::I64)),
        span,
    };
    assert_eq!(format_operands(&[spec, array]), None);
}

// d11. the base case: `panic(...)` as a whole statement. Nothing follows
// it in its block, so the terminator it emits is safe.
#[test]
fn d11_statement_panic_is_eligible() {
    let src = "fn f(n: i64) -> i64 {
                       if n < 0 { panic(\"negative\"); }
                       return n;
                   }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// d12. the formatted form goes through the same operand vetting as
// `format`, so its message can interpolate.
#[test]
fn d12_formatted_panic_is_eligible() {
    let src = "fn f(a: i64, b: i64) -> i64 {
                       if b == 0 { panic(\"cannot divide {} by {}\", a, b); }
                       return a / b;
                   }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// d13. the one-argument form takes an already-built `String`, so the
// message may be any expression the walker can emit — including one that
// allocates.
#[test]
fn d13_single_argument_panic_takes_a_computed_message() {
    let src = "fn f(n: i64) -> i64 {
                       if n <= 0 { panic(\"bad value: \" + n.toString()); }
                       return n;
                   }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// d14. Diverging operands terminate evaluation before the consuming call.
// Lowering emits the panic without fabricating a value for println.
#[test]
fn d14_operand_position_panic_is_eligible() {
    let src = "fn f() -> i64 { println(panic(\"no\")); return 1; }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// d15. a local binding named `panic` is an ordinary indirect call through
// a function value, not the builtin. Treating it as the builtin would emit
// an unwind where the program expects a call.
#[test]
fn d15_a_local_named_panic_is_not_the_builtin() {
    let span = crate::diagnostics::Span::dummy();
    let call = HirExpr {
        kind: HirExprKind::Call {
            callee: "panic".to_string().into(),
            args: Vec::new(),
        },
        ty: Type::Never,
        span,
    };
    let tables = empty_tables();
    let local = Type::Fn(Vec::new(), Box::new(Type::Never));
    tables.with_ctx(|ctx| {
        assert!(supported_panic(&call, ctx, &HashMap::new()));
        let names: HashMap<&str, Cow<'_, Type>> = HashMap::from([("panic", Cow::Borrowed(&local))]);
        assert!(!supported_panic(&call, ctx, &names));
    });
}

// d16. a `match` arm may end in a panic while its siblings produce values.
// The arm is the tail of its own block, so the position rule holds.
#[test]
fn d16_a_panicking_arm_beside_value_arms() {
    let src = "fn f(n: i64) -> String {
                       return match n {
                           1 => \"low\",
                           2 => \"high\",
                           _ => panic(\"no level {}\", n),
                       };
                   }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// d17. when EVERY arm leaves, the `match` itself is typed `!` and has no
// reachable merge. The emitter must not read the result variable there.
#[test]
fn d17_all_arms_return_is_eligible_as_a_statement() {
    let src = "fn f(n: i64) -> String {
                       match n {
                           0 => return \"zero\",
                           1 => return \"one\",
                           _ => return \"many\",
                       }
                   }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// d18. a diverging arm may run effect statements first — they precede the
// terminator in the same block, which is legal.
#[test]
fn d18_a_diverging_arm_may_run_effects_first() {
    let src = "fn f(n: i64) -> String {
                       match n {
                           0 => { println(\"zero\"); return \"z\"; }
                           _ => { println(\"other\"); println(n); return \"o\"; }
                       }
                   }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// d19. a `let` may too. The emitter brackets
// the arm — `vars` and the GC root depth are snapshotted before the body and
// restored after — so the binding is scoped to the arm rather than leaked
// into the flat map.
#[test]
fn d19_a_let_in_a_diverging_arm_is_admitted() {
    let src = "fn f(n: i64) -> i64 {
                       match n {
                           0 => { let t = 1; return t; }
                           _ => return 2,
                       }
                   }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// d20. an arm's `return` value is checked against the FUNCTION's declared
// return type, which is why `LirTypeCtx` carries it. Boxing a class into
// the declared interface needs a vtable; without one the arm is refused
// instead of returning a raw pointer.
#[test]
fn d20_arm_returns_are_checked_against_the_declared_return_type() {
    let src = format!(
        "{NAMED} fn f(b: bool) -> Named {{
                 match b {{
                     true => return new Item(\"x\"),
                     _ => return new Item(\"y\"),
                 }}
             }}"
    );
    let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
    assert!(
        tables
            .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
            .is_none()
    );

    tables.vtables.clear();
    assert!(
        tables
            .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
            .is_some(),
        "without a vtable the arm cannot box its return value"
    );
}

// d21. the position rule again, one level up: a `!`-typed `match` is
// admitted as a statement but never as an operand.
#[test]
fn d21_a_never_typed_match_is_statement_only() {
    let src = "fn f(n: i64) -> i64 {
                       match n {
                           0 => return 1,
                           _ => return 2,
                       }
                   }";
    let (p, tables) = checked_lowering(src, &["f"]);
    let f = p
        .functions
        .iter()
        .find(|f| f.name.to_string() == "f")
        .expect("lowered");
    tables.with_ctx(|ctx| {
        let ctx = &LirTypeCtx {
            return_type: &f.return_type,
            ..*ctx
        };
        assert!(lir_supported_function(f, ctx));
        assert!(
            f.blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(inst, LirInst::MatchTest { .. }))
        );
        assert!(
            !f.blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(inst, LirInst::Unsupported { .. }))
        );
        // The expression validator still rejects a divergent match as an
        // operand; lowering only moves its statement control flow to CFG.
        let inst = returned_hir_expr(
            "fn f(n: i64) -> i64 { return match n { 0 => return 1, _ => return 2 }; }",
        );
        let i64_ty = Type::I64;
        let names = HashMap::from([("n", Cow::Borrowed(&i64_ty))]);
        assert_eq!(inst.ty, Type::Never);
        assert!(supported_divergent_expr(&inst, ctx, &names));
        assert!(!supported_expr(&inst, ctx, &names));
    });
}

// d22. divergence nests: an arm whose tail is itself an all-returning
// `match` still ends its block exactly once.
#[test]
fn d22_diverging_arms_nest() {
    let src = "fn f(row: i64, col: i64) -> String {
                       match row {
                           0 => match col {
                               0 => return \"origin\",
                               _ => return \"top\",
                           },
                           _ => return \"body\",
                       }
                   }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// d23. the reason finder walks the same subset. An admitted diverging arm
// must not be blamed for a rejection that lives elsewhere, or the
// diagnostic points at working code.
#[test]
fn d23_the_reason_does_not_blame_an_admitted_diverging_arm() {
    let src = "class Config { pub static version: i64 = 7; }
                   fn unknown() -> i64 { println(0); return 9; }
                   fn f(n: i64) -> i64 {
                       match n {
                           0 => return unknown(),
                           _ => return Config::version,
                       }
                   }";
    let reason = rejected(src, "f", &["f"]);
    assert!(
        reason.contains("the call to `unknown`") && !reason.contains("version"),
        "the reason must name only the unsupported sibling, got: {reason}"
    );
}

// d24. an arm-less `match` has no arm to jump to and no value to merge, so
// it is refused before any arm-shaped reasoning runs.
#[test]
fn d24_an_empty_match_is_refused() {
    let span = crate::diagnostics::Span::dummy();
    let scrutinee = HirExpr {
        kind: HirExprKind::Var("n".to_string()),
        ty: Type::I64,
        span,
    };
    let empty = HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(scrutinee),
            arms: Vec::new(),
        },
        ty: Type::I64,
        span,
    };
    let tables = empty_tables();
    let i64_ty = Type::I64;
    tables.with_ctx(|ctx| {
        let names: HashMap<&str, Cow<'_, Type>> = HashMap::from([("n", Cow::Borrowed(&i64_ty))]);
        assert!(!supported_expr(&empty, ctx, &names));
    });
}

// d25. a VALUE-producing `match` whose arms all leave would reach its
// merge with no predecessor, so `use_var` there has no reaching
// definition. Such a shape is refused rather than emitted.
#[test]
fn d25_a_value_match_with_only_diverging_arms_is_refused() {
    let span = crate::diagnostics::Span::dummy();
    let arm = |value: i64| HirMatchArm {
        pattern: HirPattern::LiteralInt(value),
        guard: None,
        body: vec![HirStmt::Return {
            value: Some(HirExpr {
                kind: HirExprKind::Int(value),
                ty: Type::I64,
                span,
            }),
            span,
        }],
        ty: Type::Never,
        span,
    };
    let mut arms = vec![arm(0)];
    arms.push(HirMatchArm {
        pattern: HirPattern::Wildcard,
        guard: None,
        ..arm(1)
    });
    let scrutinee = HirExpr {
        kind: HirExprKind::Var("n".to_string()),
        ty: Type::I64,
        span,
    };
    let as_value = HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(scrutinee),
            arms,
        },
        // typed as a VALUE even though nothing can flow to the merge
        ty: Type::I64,
        span,
    };
    let mut tables = empty_tables();
    tables.ret = Type::I64;
    let i64_ty = Type::I64;
    tables.with_ctx(|ctx| {
        let names: HashMap<&str, Cow<'_, Type>> = HashMap::from([("n", Cow::Borrowed(&i64_ty))]);
        assert!(!supported_expr(&as_value, ctx, &names));
    });
}

// d26. `arm_diverges` reads the arm's TAIL statement. A `return` in the
// middle of an arm is not a tail, and an arm ending in an ordinary value
// does not leave.
#[test]
fn d26_arm_divergence_is_decided_by_the_tail_statement() {
    let span = crate::diagnostics::Span::dummy();
    let value = HirExpr {
        kind: HirExprKind::Int(1),
        ty: Type::I64,
        span,
    };
    let never = HirExpr {
        kind: HirExprKind::Call {
            callee: "panic".to_string().into(),
            args: Vec::new(),
        },
        ty: Type::Never,
        span,
    };
    let arm = |body: Vec<HirStmt>| HirMatchArm {
        pattern: HirPattern::Wildcard,
        guard: None,
        body,
        ty: Type::Never,
        span,
    };
    assert!(arm_diverges(&arm(vec![HirStmt::Return {
        value: Some(value.clone()),
        span,
    }])));
    assert!(arm_diverges(&arm(vec![HirStmt::Expr(never.clone())])));
    assert!(!arm_diverges(&arm(vec![HirStmt::Expr(value.clone())])));
    assert!(!arm_diverges(&arm(Vec::new())));
    // a `return` that is not the tail does not make the ARM diverge
    assert!(!arm_diverges(&arm(vec![
        HirStmt::Return {
            value: Some(value.clone()),
            span,
        },
        HirStmt::Expr(value),
    ])));
}

// d27. the return type in `LirTypeCtx` is per FUNCTION, not per program:
// `lir_rejection_reason` rebinds it from the function it was handed, so
// two functions in one module are each checked against their own.
#[test]
fn d27_the_context_return_type_is_rebound_per_function() {
    let src = "fn as_int(n: i64) -> i64 { match n { 0 => return 1, _ => return 2, } }
                   fn as_text(n: i64) -> String {
                       match n { 0 => return \"a\", _ => return \"b\", }
                   }";
    let fns = &["as_int", "as_text"];
    for name in fns {
        assert!(eligible_checked(src, name, fns), "`{name}` must be in");
    }
}

// ── control flow inside an HIR island ─────────────────

// d28. the bead's own repro: a guard that ends one path with a `panic`
// inside an arm body. Only the LIR block graph carries a function-level
// `if`, so an arm's has to be admitted and emitted on its own.
#[test]
fn d28_a_guard_inside_an_arm_body_is_admitted() {
    let src = "enum Shape { Square(i64), Circle(i64) }
                   fn f(s: Shape) -> i64 {
                       match s {
                           Shape::Square(side) => {
                               if side <= 0 { panic(\"bad\"); }
                               return side * side;
                           }
                           Shape::Circle(r) => { return r; }
                       }
                   }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// d29. the same `if`, minus the arm: a function-level guard was always in
// the subset, so d28 must not be passing because the rule got looser for
// everything.
#[test]
fn d29_a_guard_outside_a_match_was_already_admitted() {
    let src = "fn f(side: i64) -> i64 {
                       if side <= 0 { panic(\"bad\"); }
                       return side * side;
                   }";
    assert!(eligible_checked(src, "f", &["f"]));
}

// d30. an `if` in an arm is refused for what it CONTAINS, not for being an
// `if`, and the reason finder names the offending node inside the branch
// rather than blaming the guard around it.
#[test]
fn d30_an_arm_guard_is_still_checked_through() {
    let src = "class Config { pub static version: i64 = 7; }
                   fn unknown() -> i64 { println(0); return 9; }
                   fn f(n: i64) -> i64 {
                       match n {
                           0 => { if n > 0 { return unknown(); } return 1; }
                           _ => { return Config::version; }
                       }
                   }";
    let reason = rejected(src, "f", &["f"]);
    assert!(
        reason.contains("the call to `unknown`"),
        "the reason must name the unsupported call inside the guard, got: {reason}"
    );
}

// d31. an `if` both of whose branches leave ends the arm, so the arm hands
// nothing to the merge block. `body_diverges` has to see that through the
// `if`, which neither a `return` tail nor a `!` type would show.
#[test]
fn d31_an_if_ends_an_arm_only_when_both_branches_leave() {
    let span = crate::diagnostics::Span::dummy();
    let value = HirExpr {
        kind: HirExprKind::Int(1),
        ty: Type::I64,
        span,
    };
    let cond = HirExpr {
        kind: HirExprKind::Bool(true),
        ty: Type::Bool,
        span,
    };
    let returns = || {
        vec![HirStmt::Return {
            value: Some(value.clone()),
            span,
        }]
    };
    let guard = |else_branch: Option<Vec<HirStmt>>| {
        vec![HirStmt::If {
            cond: cond.clone(),
            then_branch: returns(),
            else_branch,
            span,
        }]
    };
    assert!(body_diverges(&guard(Some(returns()))));
    // no `else`: the falling-through path is still a path
    assert!(!body_diverges(&guard(None)));
    // an `else` that does not leave is one too
    assert!(!body_diverges(&guard(Some(vec![HirStmt::Expr(
        value.clone()
    )]))));
}

// d32. a `match` every arm of which leaves is admissible where nothing
// follows it, and refused as an OPERAND — where something after it would
// read the value no path can produce. The checker's own type for the
// `match` is not what decides this: it types the same shape `!` in one
// position and `void` in another.
#[test]
fn d32_an_all_leaving_match_is_admitted_only_where_nothing_follows() {
    let src = "enum Shape { Square(i64), Circle(i64) }
                   fn f(s: Shape) -> i64 {
                       match s {
                           Shape::Square(side) => { return side * side; }
                           Shape::Circle(r) => { panic(\"no circles\"); }
                       }
                   }";
    assert!(eligible_checked(src, "f", &["f"]));

    let span = crate::diagnostics::Span::dummy();
    let arm = |value: i64| HirMatchArm {
        pattern: HirPattern::LiteralInt(value),
        guard: None,
        body: vec![HirStmt::Return {
            value: Some(HirExpr {
                kind: HirExprKind::Int(value),
                ty: Type::I64,
                span,
            }),
            span,
        }],
        ty: Type::Never,
        span,
    };
    let mut arms = vec![arm(0)];
    arms.push(HirMatchArm {
        pattern: HirPattern::Wildcard,
        guard: None,
        ..arm(1)
    });
    assert!(match_diverges(&arms));
    let as_operand = HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(HirExpr {
                kind: HirExprKind::Var("n".to_string()),
                ty: Type::I64,
                span,
            }),
            arms,
        },
        ty: Type::I64,
        span,
    };
    let mut tables = empty_tables();
    tables.ret = Type::I64;
    let i64_ty = Type::I64;
    tables.with_ctx(|ctx| {
        let names: HashMap<&str, Cow<'_, Type>> = HashMap::from([("n", Cow::Borrowed(&i64_ty))]);
        assert!(!supported_expr(&as_operand, ctx, &names));
        assert!(supported_divergent_expr(&as_operand, ctx, &names));
    });
}

// ── preemption safepoints on loop back edges ───────────
//
// A cooperative poll fn parks at a safepoint, and a local the async liveness
// pass left in an SSA value cannot survive that park. The pass models
// suspension at loop BACK EDGES and at statements that execute a call, so
// `lir_back_edges` has to name exactly the loop-closing edges — no more.

fn back_edges_of(src: &str, name: &str, fns: &[&str]) -> Vec<(usize, usize)> {
    let (f, _tables) = lir_fn_and_tables(src, name, fns);
    let mut edges: Vec<(usize, usize)> = lir_back_edges(&f).into_iter().collect();
    edges.sort_unstable();
    edges
}

fn terminator_safepoints(src: &str, name: &str, fns: &[&str]) -> Vec<usize> {
    let (f, _tables) = lir_fn_and_tables(src, name, fns);
    let back = lir_back_edges(&f);
    f.blocks
        .iter()
        .filter(|b| lir_terminator_needs_preempt_safepoint(b, &back))
        .map(|b| b.id.0)
        .collect()
}

/// Branch blocks whose `then` successor is laid out next,
/// every branch block, and each block's loop depth.
fn then_fallthroughs(src: &str, name: &str) -> (Vec<usize>, Vec<usize>, Vec<u32>) {
    then_fallthroughs_in(src, name, &[name])
}

fn then_fallthroughs_in(src: &str, name: &str, fns: &[&str]) -> (Vec<usize>, Vec<usize>, Vec<u32>) {
    let (f, _tables) = lir_fn_and_tables(src, name, fns);
    let depths = super::super::block_layout::lir_loop_depths(&f);
    let flipped = (0..f.blocks.len())
        .filter(|&i| super::super::block_layout::prefer_then_fallthrough(&f, &depths, i))
        .collect();
    let branches = f
        .blocks
        .iter()
        .filter(|b| matches!(b.terminator, Terminator::Branch { .. }))
        .map(|b| b.id.0)
        .collect();
    (flipped, branches, depths.all)
}

// Lowering repeats a `while` condition in several copies of the body;
// every copy's exit test must keep the next iteration as fall-through.
#[test]
fn layout_while_body_follows_its_header() {
    let src = "fn f(n: i64) -> i64 {
                       let mut i = 0;
                       while i < n { i = i + 1; }
                       return i;
                   }";
    let (flipped, branches, depths) = then_fallthroughs(src, "f");
    let (latch, header) = back_edges_of(src, "f", &["f"])[0];
    assert!(flipped.contains(&header), "{depths:?}");
    assert_eq!(flipped, branches, "{depths:?}");
    assert_eq!(depths[header], 1);
    assert_eq!(depths[latch], 1);
    assert_eq!(depths[0], 0);
    assert_eq!(depths.iter().filter(|&&depth| depth == 0).count(), 2);
}

#[test]
fn layout_keeps_branches_outside_loops_and_break_arms_in_source_order() {
    let straight = "fn f(c: bool) -> i64 {
                            let mut v = 0;
                            if c { v = 1; } else { v = 2; }
                            return v;
                        }";
    let (flipped, branches, depths) = then_fallthroughs(straight, "f");
    assert!(!branches.is_empty());
    assert!(flipped.is_empty());
    assert!(depths.iter().all(|&depth| depth == 0));
    // `if c { break }` already has its in-loop arm second; `while true`
    // is a constant branch with no exit arm.
    let breaking = "fn f(n: i64) -> i64 {
                            let mut i = 0;
                            while true { if i >= n { break; } i = i + 1; }
                            return i;
                        }";
    let (flipped, branches, _) = then_fallthroughs(breaking, "f");
    assert!(!branches.is_empty());
    assert!(flipped.is_empty(), "{flipped:?}");
}

#[test]
fn layout_nested_loop_depths_prefer_the_inner_body() {
    let src = "fn f(n: i64) -> i64 {
                       let mut t = 0;
                       let mut i = 0;
                       while i < n {
                           let mut j = 0;
                           while j < n { t = t + j; j = j + 1; }
                           i = i + 1;
                       }
                       return t;
                   }";
    let (flipped, branches, depths) = then_fallthroughs(src, "f");
    assert_eq!(depths.iter().copied().max(), Some(2), "{depths:?}");
    let headers: Vec<usize> = back_edges_of(src, "f", &["f"])
        .into_iter()
        .map(|(_, header)| header)
        .collect();
    assert!(headers.len() >= 2);
    assert!(
        headers.iter().all(|header| flipped.contains(header)),
        "{depths:?}"
    );
    // Every branch here is a loop condition whose exit leaves a loop.
    assert_eq!(flipped, branches, "{depths:?}");
}

// A loop that calls keeps source order: Cranelift then
// visits its exit before its body and leaves the body's constant callee
// addresses at their uses instead of holding each in a callee-saved
// register across the calls. A call-free loop inside it is still oriented.
#[test]
fn layout_orients_only_call_free_loops() {
    let direct = "fn g(x: i64) -> i64 { return x + 1; }
                      fn f(n: i64) -> i64 {
                          let mut t = 0;
                          let mut i = 0;
                          while i < n {
                              let mut j = 0;
                              while j < n { t = t + j; j = j + 1; }
                              t = t + g(i);
                              i = i + 1;
                          }
                          return t;
                      }";
    let (flipped, branches, depths) = then_fallthroughs_in(direct, "f", &["f", "g"]);
    let edges = back_edges_of(direct, "f", &["f", "g"]);
    let inner: Vec<usize> = edges
        .iter()
        .map(|&(_, header)| header)
        .filter(|&header| depths[header] == 2)
        .collect();
    let outer: Vec<usize> = edges
        .iter()
        .map(|&(_, header)| header)
        .filter(|&header| depths[header] == 1)
        .collect();
    assert!(!inner.is_empty() && !outer.is_empty(), "{depths:?}");
    assert!(
        inner.iter().all(|header| flipped.contains(header)),
        "{flipped:?} {depths:?}"
    );
    assert!(
        outer.iter().all(|header| !flipped.contains(header)),
        "{flipped:?} {depths:?}"
    );
    assert!(flipped.len() < branches.len());

    // The virtual_dispatch shape: guarded interface calls in the body.
    let dispatch = "import std::collections::Array;
                        interface Shape { fn area(self) -> i64; }
                        class Square implements Shape {
                            pub side: i64;
                            pub fn area(self) -> i64 { return self.side * self.side; }
                        }
                        fn f(shapes: Array<Shape>, n: i64) -> i64 {
                            let mut sum = 0;
                            let mut i = 0;
                            while i < n { sum = sum + shapes[i].area(); i = i + 1; }
                            return sum;
                        }";
    let (flipped, branches, depths) = then_fallthroughs(dispatch, "f");
    assert!(depths.iter().any(|&depth| depth > 0));
    assert!(!branches.is_empty());
    assert!(flipped.is_empty(), "{flipped:?} {depths:?}");
}

/// (unrooted temporaries, synthetic array temporaries) of `f`.
fn unrooted_count(src: &str) -> (usize, usize) {
    let (f, _) = lir_fn_and_tables(src, "f", &["f", "g"]);
    let temps = f
        .locals
        .iter()
        .filter(|local| local.synthetic && !local.parameter)
        .filter(|local| super::super::transient_roots::is_array(&local.ty))
        .count();
    let unrooted = super::super::transient_roots::unrooted_array_temps(&f);
    for id in &unrooted {
        let local = &f.locals[id.0 as usize];
        assert!(local.synthetic && !local.parameter, "{local:?}");
        assert!(super::super::transient_roots::is_array(&local.ty));
    }
    (unrooted.len(), temps)
}

//: receiver temporaries read only by `len`/scalar indexing in
// a GC-free run need no root.
#[test]
fn transient_array_temps_skip_roots_only_inside_gc_free_runs() {
    let plain = "import std::collections::Array;
                     fn g(i: i64) -> i64 { println(i); return i; }
                     fn f(xs: Array<i64>) -> i64 {
                         let mut i = 0;
                         let mut s = 0;
                         while i < xs.len() { s = s + xs[i]; i = i + 1; }
                         return s;
                     }";
    // The condition's receiver copy for `len`.
    assert_eq!(unrooted_count(plain), (1, 1));
    // An index operand that needs a call copies the receiver first; the
    // call is a GC point, so that copy keeps its root.
    assert_eq!(unrooted_count(&plain.replace("xs[i]", "xs[g(i)]")), (1, 2));
    // Interface boxing between the copy and its read is a GC point.
    let boxed = "import std::collections::Array;
                     interface V { fn v() -> i64; }
                     class B implements V { pub n: i64; pub fn v() -> i64 { return self.n; } }
                     fn g(i: i64) -> i64 { return i; }
                     fn f(xs: Array<i64>) -> i64 {
                         let mut s = 0;
                         let mut i = 0;
                         while i < xs.len() { let b: V = new B(i); s = s + b.v() + xs[i]; i = i + 1; }
                         return s;
                     }";
    let (f, _) = lir_fn_and_tables(boxed, "f", &["f", "g"]);
    let boxing = f
        .blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .filter(|inst| matches!(inst, LirInst::Let { .. }))
        .filter(|inst| !super::super::transient_roots::gc_free(&f, inst))
        .count();
    assert!(boxing >= 1);
}

//: a scalar element store only inspects the receiver copy,
// like `len` and scalar reads; a GC point before the store, or a
// non-scalar element, keeps the copy rooted.
#[test]
fn transient_array_temps_cover_scalar_stores() {
    let plain = "import std::collections::Array;
                     fn g(i: i64) -> i64 { println(i); return i; }
                     fn f(xs: Array<i64>) -> i64 {
                         let mut i = 0;
                         while i < xs.len() { xs[i] = i + 1; i = i + 1; }
                         return i;
                     }";
    // The condition's `len` copy and the store's receiver copy.
    assert_eq!(unrooted_count(plain), (2, 2));
    // The value calls between the copy and the store.
    assert_eq!(
        unrooted_count(&plain.replace("xs[i] = i + 1", "xs[i] = g(i)")),
        (1, 2)
    );
    // The index calls too.
    assert_eq!(
        unrooted_count(&plain.replace("xs[i] = i + 1", "xs[g(i)] = i")),
        (1, 2)
    );
    for ty in ["f64", "bool"] {
        let value = if ty == "f64" { "1.5" } else { "true" };
        let src = plain
            .replace("Array<i64>", &format!("Array<{ty}>"))
            .replace("xs[i] = i + 1", &format!("xs[i] = {value}"));
        assert_eq!(unrooted_count(&src), (2, 2), "{ty}");
    }
    // A reference element store may run a barrier: it stays rooted.
    let strings = "import std::collections::Array;
                       fn g(i: i64) -> i64 { return i; }
                       fn f(xs: Array<String>) -> i64 {
                           let mut i = 0;
                           while i < xs.len() { xs[i] = \"s\"; i = i + 1; }
                           return i;
                       }";
    assert_eq!(unrooted_count(strings), (1, 2));
    // The scalar store itself is GC-free; a reference store is not.
    let stores = |src: &str| {
        let (f, _) = lir_fn_and_tables(src, "f", &["f", "g"]);
        f.blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter(|inst| {
                matches!(
                    inst,
                    LirInst::Compute {
                        value: crate::ir::lowered::LirRvalue::ArrayStore { .. },
                        ..
                    }
                )
            })
            .map(|inst| super::super::transient_roots::gc_free(&f, inst))
            .collect::<Vec<_>>()
    };
    assert_eq!(stores(plain), [true]);
    assert_eq!(stores(strings), [false]);
}

#[test]
fn transient_runs_inherit_only_along_sole_plain_edges() {
    let looped = "import std::collections::Array;
                      fn f(xs: Array<i64>) -> i64 {
                          let mut i = 0;
                          let mut s = 0;
                          while i < xs.len() { s = s + xs[i]; i = i + 1; }
                          return s;
                      }";
    let (f, _) = lir_fn_and_tables(looped, "f", &["f"]);
    let preds = super::super::transient_roots::sole_predecessors(&f);
    assert_eq!(preds[0], None);
    let (_, header) = back_edges_of(looped, "f", &["f"])[0];
    // The header joins the entry and the latch.
    assert_eq!(preds[header], None);
    for (block, pred) in preds.iter().enumerate() {
        if let Some(pred) = *pred {
            assert!(lir_block_successors(&f.blocks[pred]).contains(&block));
        }
    }
    assert!(preds.iter().any(Option::is_some));
    // Defers resume blocks outside the terminators: nothing inherits.
    let deferred = looped.replace("let mut i = 0;", "defer println(\"x\"); let mut i = 0;");
    let (f, _) = lir_fn_and_tables(&deferred, "f", &["f"]);
    assert!(
        super::super::transient_roots::sole_predecessors(&f)
            .iter()
            .all(Option::is_none)
    );
}

#[test]
fn transient_reuse_carries_a_bounded_fact_set() {
    use cranelift_codegen::entity::EntityRef;
    use cranelift_codegen::ir::{StackSlot, Value};
    let mut reuse = super::super::transient_roots::LirReuse::default();
    let facts = super::super::transient_roots::ArrayFacts {
        len: Value::new(0),
        buffer: None,
    };
    for k in 0..16 {
        if k % 2 == 0 {
            reuse.slots.insert(StackSlot::new(k), Value::new(k));
        } else {
            reuse.arrays.insert(Value::new(k), facts);
        }
    }
    assert!(reuse.carriable());
    reuse.arrays.insert(Value::new(99), facts);
    assert!(!reuse.carriable());
}

// s1. straight-line code closes no loop.
#[test]
fn s1_straight_line_has_no_back_edge() {
    let src = "fn f(a: i64) -> i64 { let b = a + 1; return b; }";
    assert!(back_edges_of(src, "f", &["f"]).is_empty());
}

// s2. the regression itself: lowering numbers an `if`'s JOIN block before
// the `else` arm, so the else arm's jump to the join runs backwards by id.
// It closes no loop, so it must not be a back edge.
#[test]
fn s2_if_else_join_is_not_a_back_edge() {
    let src = "fn f(c: bool) -> i64 {
                       let mut v = 0;
                       if c { v = 1; } else { v = 2; }
                       return v;
                   }";
    assert!(
        back_edges_of(src, "f", &["f"]).is_empty(),
        "an `if` join closes no loop"
    );
}

// s3.... and no terminator in that function asks for a safepoint either.
#[test]
fn s3_if_else_join_asks_for_no_safepoint() {
    let src = "fn f(c: bool) -> i64 {
                       let mut v = 0;
                       if c { v = 1; } else { v = 2; }
                       return v;
                   }";
    assert!(terminator_safepoints(src, "f", &["f"]).is_empty());
}

// s4. a `while` loop does close one, and the safepoint goes with it.
#[test]
fn s4_while_loop_has_one_back_edge() {
    let src = "fn f(n: i64) -> i64 {
                       let mut i = 0;
                       while i < n { i = i + 1; }
                       return i;
                   }";
    let edges = back_edges_of(src, "f", &["f"]);
    assert_eq!(edges.len(), 1, "{edges:?}");
    let (from, to) = edges[0];
    assert!(to < from, "a back edge runs backwards: {edges:?}");
    assert!(terminator_safepoints(src, "f", &["f"]).contains(&from));
}

// s5. a loop nested in a loop closes two, one per level.
#[test]
fn s5_nested_loops_close_one_edge_each() {
    let src = "fn f(n: i64) -> i64 {
                       let mut total = 0;
                       let mut i = 0;
                       while i < n {
                           let mut j = 0;
                           while j < n { total = total + 1; j = j + 1; }
                           i = i + 1;
                       }
                       return total;
                   }";
    assert_eq!(back_edges_of(src, "f", &["f"]).len(), 2);
}

// s6. a loop whose body branches still closes exactly one edge: the `if`
// join inside it is a forward merge, not a second loop.
#[test]
fn s6_branch_inside_a_loop_adds_no_back_edge() {
    let src = "fn f(n: i64) -> i64 {
                       let mut total = 0;
                       let mut i = 0;
                       while i < n {
                           if i % 2 == 0 { total = total + i; } else { total = total + 1; }
                           i = i + 1;
                       }
                       return total;
                   }";
    assert_eq!(back_edges_of(src, "f", &["f"]).len(), 1);
}

// s7. `continue` jumps to the loop head, which is a back edge of its own.
#[test]
fn s7_continue_closes_the_loop_too() {
    let src = "fn f(n: i64) -> i64 {
                       let mut i = 0;
                       let mut total = 0;
                       while i < n {
                           i = i + 1;
                           if i % 2 == 0 { continue; }
                           total = total + i;
                       }
                       return total;
                   }";
    let edges = back_edges_of(src, "f", &["f"]);
    assert!(
        edges.len() >= 2,
        "continue adds an edge to the head: {edges:?}"
    );
    for (from, to) in &edges {
        assert!(to < from, "{edges:?}");
    }
}

// s8. `break` leaves the loop forwards, so it is never a back edge.
#[test]
fn s8_break_is_a_forward_edge() {
    let src = "fn f(n: i64) -> i64 {
                       let mut i = 0;
                       while i < n {
                           if i == 3 { break; }
                           i = i + 1;
                       }
                       return i;
                   }";
    let edges = back_edges_of(src, "f", &["f"]);
    assert_eq!(edges.len(), 1, "only the loop itself closes: {edges:?}");
}

// Calls move out of return expressions into flat instructions. Async
// functions still have the explicit preemption edge inserted before them.
#[test]
fn s9_a_call_in_return_still_asks_for_a_safepoint() {
    for asynchronous in [false, true] {
        let source = format!(
            "fn g(x: i64) -> i64 {{ return 1 / x; }} {}fn f() -> i64 {{ return g(1); }}",
            if asynchronous { "async " } else { "" }
        );
        let (program, _) = checked_lowering(&source, &["f", "g"]);
        let function = program
            .functions
            .iter()
            .find(|f| f.name.is_free_named("f"))
            .unwrap();
        assert!(
            function
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(
                    inst,
                    LirInst::Compute {
                        value: crate::ir::lowered::LirRvalue::DirectCall { .. },
                        ..
                    }
                ))
        );
        if asynchronous {
            assert!(function.blocks.iter().any(|block| matches!(
                block.terminator,
                Terminator::Suspend {
                    operation: SuspendOp::Preempt,
                    ..
                }
            )));
        }
    }
}

// s10. a plain `return` of a value that calls nothing asks for none.
#[test]
fn s10_a_valueless_return_asks_for_no_safepoint() {
    let src = "fn f(a: i64) -> i64 { return a + 1; }";
    assert!(terminator_safepoints(src, "f", &["f"]).is_empty());
}

// -- splitting an await out of value position ---------
//
// The walker emits a value-position await BEFORE the rest of its statement,
// because a Cranelift value computed ahead of the park does not survive the
// poll return. That reorder is legal only when everything the statement
// would have evaluated first can be evaluated again afterwards, and when
// the await is reached unconditionally. `lir_hoistable_around` is the
// predicate that decides both.

/// The value returned by `name`, which must be `return <expr>;`.
fn returned_expr_of(src: &str, name: &str) -> HirExpr {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let (program, errs) = Parser::new(tokens).parse();
    assert!(errs.is_empty(), "{errs:?}");
    let (hir, diags) = crate::ir::lower::lower_program(&program);
    assert!(diags.is_empty(), "{diags:?}");
    let function = hir
        .functions
        .iter()
        .find(|f| f.name.to_string() == name)
        .expect("function present");
    match function.body.last().expect("return statement") {
        HirStmt::Return {
            value: Some(value), ..
        } => value.clone(),
        other => panic!("expected value return, got {other:?}"),
    }
}

/// Can the single suspension inside `main`'s returned expression be lifted
/// in front of it? Panics unless there is exactly one.
fn hoistable_in_main(src: &str) -> bool {
    let value = returned_expr_of(src, "main");
    let mut found = Vec::new();
    lir_collect_suspensions(&value, &mut found);
    assert_eq!(found.len(), 1, "fixture must hold exactly one suspension");
    let mut seen = false;
    lir_hoistable_around(&value, found[0], &mut seen)
}

const LEAF: &str = "async fn g() -> i64 { return 1; }\n";

// v1. only values the emitter can produce twice with the same result are
// rematerializable, so they can be re-read after a suspension.
#[test]
fn v1_literals_and_variables_are_rematerializable() {
    let src = "fn f(a: i64) -> i64 { return a; }
                   fn lit() -> i64 { return 7; }
                   fn call(a: i64) -> i64 { return f(a); }";
    assert!(lir_rematerializable(&returned_expr_of(src, "f")));
    assert!(lir_rematerializable(&returned_expr_of(src, "lit")));
    assert!(!lir_rematerializable(&returned_expr_of(src, "call")));
}

// v2. the collector finds the await nested in an operand.
#[test]
fn v2_a_nested_await_is_found() {
    let src = format!("{LEAF}async fn main() -> i64 {{ return 1 + await g(); }}");
    let value = returned_expr_of(&src, "main");
    let mut found = Vec::new();
    lir_collect_suspensions(&value, &mut found);
    assert_eq!(found.len(), 1);
    assert!(matches!(found[0].kind, HirExprKind::Await { .. }));
}

// v3. a lambda body is a separate function, so its awaits are not this
// statement's suspensions and must not be counted.
#[test]
fn v3_a_lambda_body_is_not_searched() {
    let src = "fn main() -> i64 { let f = |x: i64| -> i64 { return x; }; return f(1); }";
    let value = returned_expr_of(src, "main");
    let mut found = Vec::new();
    lir_collect_suspensions(&value, &mut found);
    assert!(found.is_empty());
}

// v4. a variable read before the await can simply be read again.
#[test]
fn v4_a_variable_operand_before_the_await_is_hoistable() {
    let src = format!("{LEAF}async fn main(n: i64) -> i64 {{ return n + await g(); }}");
    assert!(hoistable_in_main(&src));
}

// v5. a synchronous call before the await may have side effects, so
// re-running it after the park would change the program.
#[test]
fn v5_a_call_before_the_await_is_not_hoistable() {
    let src = format!(
        "{LEAF}fn side() -> i64 {{ return 2; }}
                   async fn main() -> i64 {{ return side() + await g(); }}"
    );
    assert!(!hoistable_in_main(&src));
}

// v6. everything AFTER the await already runs on the resume path, so it
// places no restriction at all.
#[test]
fn v6_a_call_after_the_await_is_hoistable() {
    let src = format!(
        "{LEAF}fn side() -> i64 {{ return 2; }}
                   async fn main() -> i64 {{ return await g() + side(); }}"
    );
    assert!(hoistable_in_main(&src));
}

// v7. `&&` may never evaluate its right operand, so lifting an await out of
// it would suspend on a path that does not run.
#[test]
fn v7_a_short_circuit_operand_is_not_hoistable() {
    let src = "async fn g() -> bool { return true; }
                   async fn main(c: bool) -> bool { return c && await g(); }";
    assert!(!hoistable_in_main(src));
}

// v8. same for a ternary arm - and for its condition, which this rejects
// conservatively rather than reasoning about position.
#[test]
fn v8_a_ternary_operand_is_not_hoistable() {
    let arm = format!("{LEAF}async fn main(c: bool) -> i64 {{ return c ? await g() : 0; }}");
    assert!(!hoistable_in_main(&arm));
    let cond = "async fn g() -> bool { return true; }
                    async fn main() -> i64 { return await g() ? 1 : 0; }";
    assert!(!hoistable_in_main(cond));
}

// v9. a `match` picks one arm, so nothing inside it is reached
// unconditionally.
#[test]
fn v9_a_match_operand_is_not_hoistable() {
    let src = format!(
        "{LEAF}async fn main(n: i64) -> i64 {{
                 return match n {{ 0 => await g(), _ => 1, }};
             }}"
    );
    assert!(!hoistable_in_main(&src));
}

// v10. the await itself is the boundary: an operand to its RIGHT in the
// same expression is fine, one to its left is judged on rematerializability
// alone, so a literal passes where a call does not.
#[test]
fn v10_the_boundary_is_the_await_not_the_expression() {
    let ok = format!("{LEAF}async fn main() -> i64 {{ return 1 + await g(); }}");
    assert!(hoistable_in_main(&ok));
    let no = format!(
        "{LEAF}fn side() -> i64 {{ return 2; }}
                   async fn main() -> i64 {{ return 1 + side() + await g(); }}"
    );
    assert!(!hoistable_in_main(&no));
}
//: explicit acceptance/rejection perspectives for the shared
// scalar fact. Cases with unsafe definitions must retain signed semantics.
#[test]
fn nonnegative_induction_proof_matrix() {
    let cases = [
        (
            "zero, strict constant",
            "let mut i = 0; while i < 10 { i = i + 1; }",
            true,
        ),
        (
            "positive start",
            "let mut i = 3; while i < 10 { i = i + 1; }",
            true,
        ),
        (
            "dynamic i64 upper bound",
            "let mut i = 0; while i < n { i = i + 1; }",
            true,
        ),
        (
            "two safe loops and reset",
            "let mut i = 0; while i < 10 { i = i + 1; } i = 0; while i < n { i = i + 1; }",
            true,
        ),
        (
            "step two, bounded",
            "let mut i = 0; while i < 10 { i = i + 2; }",
            true,
        ),
        (
            "MAX endpoint",
            "let mut i = 9223372036854775806; while i < 9223372036854775807 { i = i + 1; }",
            true,
        ),
        (
            "step two exact endpoint",
            "let mut i = 9223372036854775805; while i < 9223372036854775806 { i = i + 2; }",
            true,
        ),
        (
            "negative initial",
            "let mut i = -1; while i < 10 { i = i + 1; }",
            false,
        ),
        (
            "negative reset",
            "let mut i = 0; while i < 10 { i = i + 1; } i = -1;",
            false,
        ),
        (
            "unbounded increment",
            "let mut i = 0; while true { i = i + 1; if i == 10 { break; } }",
            false,
        ),
        (
            "inclusive MAX can wrap",
            "let mut i = 0; while i <= n { i = i + 1; }",
            false,
        ),
        (
            "large step can wrap",
            "let mut i = 0; while i < n { i = i + 2; }",
            false,
        ),
        (
            "constant step can wrap",
            "let mut i = 0; while i < 9223372036854775807 { i = i + 2; }",
            false,
        ),
        (
            "decrement",
            "let mut i = 0; while i < n { i = i - 1; }",
            false,
        ),
        (
            "zero step",
            "let mut i = 0; while i < n { i = i + 0; }",
            false,
        ),
        (
            "dynamic step",
            "let mut i = 0; while i < n { i = i + n; }",
            false,
        ),
        (
            "two increments per guard",
            "let mut i = 0; while i < n { i = i + 1; i = i + 1; }",
            false,
        ),
        (
            "assignment invalidates",
            "let mut i = 0; while i < n { i = n; i = i + 1; }",
            false,
        ),
        (
            "wrong guarded variable",
            "let mut i = 0; while n < 10 { i = i + 1; }",
            false,
        ),
        (
            "false edge",
            "let mut i = 0; if i < n {} else { i = i + 1; }",
            false,
        ),
        (
            "join is conservative",
            "let mut i = 0; while i < n { if n == 3 { println(n); } i = i + 1; }",
            false,
        ),
        (
            "mutable reference",
            "let mut i = 0; while i < n { g(&i); i = i + 1; }",
            false,
        ),
        (
            "closure capture",
            "let mut i = 0; while i < n { let f = || i; println(f()); i = i + 1; }",
            false,
        ),
        (
            "call and poll preserve scalar",
            "let mut i = 0; while i < n { println(i); i = i + 1; }",
            true,
        ),
    ];
    for (label, body, expected) in cases {
        // Keep this matrix about induction, not the pre-existing scalar unroller.
        let body = body.replace("i = i +", "println(i); i = i +");
        let source =
            format!("fn g(x: &mut i64) {{ x = -1; }} fn f(n: i64) -> i64 {{ {body} return i; }}");
        let (f, _) = lir_fn_and_tables(&source, "f", &["f", "g"]);
        let proof = super::super::nonnegative::analyze(&f);
        let i = f.locals.iter().find(|l| l.name == "i").unwrap().id;
        assert_eq!(
            proof.contains(&crate::ir::lowered::LirOperand::Local(i)),
            expected,
            "{label}: {f:#?}"
        );
    }
}

#[test]
fn nonnegative_induction_scaling() {
    for n in [8, 32, 128, 512] {
        let mut source = String::from("fn f(n: i64) -> i64 { let mut sum = 0;");
        for k in 0..n {
            source.push_str(&format!("let mut idx{k} = 0; while idx{k} < n {{ sum = sum + idx{k} % 1000; idx{k} = idx{k} + 1; }}"));
        }
        source.push_str("return sum; }");
        let (f, _) = lir_fn_and_tables(&source, "f", &["f"]);
        let proof = super::super::nonnegative::analyze(&f);
        let insts: usize = f.blocks.iter().map(|b| b.instrs.len()).sum();
        let edges: usize = f
            .blocks
            .iter()
            .map(lir_block_successors)
            .map(|s| s.len())
            .sum();
        assert!(proof.work <= 16 * (insts + edges + f.locals.len()));
        assert_eq!(
            f.locals
                .iter()
                .filter(|l| l.name.starts_with('i') && proof.locals[l.id.0 as usize])
                .count(),
            n
        );
        println!(
            "nonnegative_scale sites={n} instructions={insts} edges={edges} locals={} work={}",
            f.locals.len(),
            proof.work
        );
    }
}
#[test]
fn nonnegative_induction_adverse_shapes() {
    for shape in ["nested", "exits", "copies", "fanout"] {
        for n in [8, 32, 128] {
            let mut source = String::from("fn f(n: i64) -> i64 { let mut sum = 0;");
            if shape == "nested" {
                for k in 0..n {
                    source.push_str(&format!("let mut idx{k} = 0; while idx{k} < n {{"));
                }
                source.push_str("println(n);");
                for k in (0..n).rev() {
                    source.push_str(&format!("idx{k} = idx{k} + 1; }}"));
                }
            } else if shape == "exits" {
                source.push_str("let mut idx = 0; while idx < n {");
                for k in 0..n {
                    source.push_str(&format!("if idx == {k} {{ return idx; }}"));
                }
                source.push_str("idx = idx + 1; }");
            }
            source.push_str("return sum; }");
            let (mut f, _) = lir_fn_and_tables(&source, "f", &["f"]);
            if shape == "copies" || shape == "fanout" {
                let base = f.locals.iter().find(|l| l.name == "sum").unwrap().clone();
                let mut previous = base.id;
                for k in 0..n {
                    let mut local = base.clone();
                    local.id = LirLocalId(f.locals.len() as u32);
                    local.name = format!("__proof_copy_{k}");
                    local.synthetic = true;
                    f.blocks[0].instrs.push(LirInst::Compute {
                        local: local.id,
                        value: crate::ir::lowered::LirRvalue::Use(
                            crate::ir::lowered::LirOperand::Local(previous),
                        ),
                        span: Span::dummy(),
                    });
                    if shape == "copies" {
                        previous = local.id;
                    }
                    f.locals.push(local);
                }
            }
            let proof = super::super::nonnegative::analyze(&f);
            let insts: usize = f.blocks.iter().map(|b| b.instrs.len()).sum();
            let edges: usize = f
                .blocks
                .iter()
                .map(lir_block_successors)
                .map(|s| s.len())
                .sum();
            assert!(proof.work <= 16 * (insts + edges + f.locals.len()));
            if shape == "copies" || shape == "fanout" {
                assert_eq!(proof.locals.iter().filter(|&&known| known).count(), n + 1);
            }
            println!(
                "nonnegative_scale shape={shape} size={n} instructions={insts} edges={edges} locals={} work={}",
                f.locals.len(),
                proof.work
            );
        }
    }
}

#[test]
fn nonnegative_induction_rejects_async_capture_and_unrolled_definitions() {
    let source =
        "fn f(n: i64) -> i64 { let mut i = 0; while i < n { println(i); i = i + 1; } return i; }";
    let (mut f, _) = lir_fn_and_tables(source, "f", &["f"]);
    assert!(
        super::super::nonnegative::analyze(&f)
            .locals
            .iter()
            .any(|v| *v)
    );
    f.is_async = true;
    assert!(
        super::super::nonnegative::analyze(&f)
            .locals
            .iter()
            .all(|v| !v)
    );
    f.is_async = false;
    let i = f.locals.iter().find(|l| l.name == "i").unwrap().id;
    f.async_frame.locals.insert(
        i,
        crate::ir::lowered::async_liveness::FrameSlot { index: 0 },
    );
    assert!(
        super::super::nonnegative::analyze(&f)
            .locals
            .iter()
            .all(|v| !v)
    );
    f.async_frame.locals.clear();
    f.captures.push(crate::ir::lowered::LirCapture {
        name: "i".into(),
        source: "i".into(),
        ty: Type::I64,
    });
    assert!(
        super::super::nonnegative::analyze(&f)
            .locals
            .iter()
            .all(|v| !v)
    );
    let (f, _) = lir_fn_and_tables(
        "fn f() -> i64 { let mut i = 0; while i < 10 { i = i + 1; } return i; }",
        "f",
        &["f"],
    );
    let i = f.locals.iter().find(|l| l.name == "i").unwrap().id;
    assert!(
        !super::super::nonnegative::analyze(&f).contains(&crate::ir::lowered::LirOperand::Local(i))
    );
}
