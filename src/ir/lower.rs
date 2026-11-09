//! Lowering: type-checked AST → typed HIR ([`super::typed_ast`]) — typed HIR lowering.
//!
//! Coverage so far: the MVP-core constructs (literals, variables, arithmetic/
//! comparison/logical/unary operators, free-function calls, `print`, and
//! the `let`/assign/`if`/`while`/`return` statements); array literals, indexing,
//! and the ternary operator; classes — `new`, object literals, field access,
//! method calls (instance members resolved along the base-class chain, so
//! inheritance works), static field reads and static calls, and class method
//! bodies (receiver bound as `self`); array `for` loops; and field/index/static
//! assignment statements. Type information flows in through a [`LowerCtx`]
//! (parameter/`let` bindings, free-function return types, and per-class
//! field/method/static-member types) and is attached to every [`HirExpr`], so a
//! downstream consumer never has to re-derive a type from the AST. Also
//! covered: ranges and range `for` (`Range<i64>`, i64 elements), async calls
//! (`Task<T>` at the call site) and `await` (unwraps `Task`/`Future`), `?`
//! propagation (unwraps `Result`/`Option`), annotated lambdas (`fn(..) -> R`
//! types, indirect calls through fn-typed variables), constructors (lowered as
//! `init` with `self` bound) with `super.init`, and the checker's builtin
//! functions (seeded registry). Constructs not yet covered (`match`, maps,
//! unannotated lambda params, and generic substitution) return a diagnostic
//! rather than silently dropping work, so later slices can extend coverage
//! incrementally without changing behavior.

mod expressions;
mod resolution;

#[cfg(test)]
thread_local! {
    static REQUIRED_TYPE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

use expressions::{lower_expr, lower_expr_cont};
use resolution::lower_resolution_partial;

use crate::qualified_name::QualifiedName;
use std::collections::{HashMap, HashSet};

use crate::diagnostics::{Diagnostic, ErrorCode, Severity, Span};
use crate::parser::ast::{
    AwaitExpr, BinOp, BinaryExpr, Block, CallArg, CallArgMode, CallExpr, DeferBody, Expr, ExprId,
    FunctionDecl, Item, LambdaExpr, MethodCallExpr, MethodDecl, NewExpr, ObjectLiteralExpr,
    PatternId, Program, RangeExpr, SelectCaseKind, SelectExpr, StaticCallExpr, StaticFieldExpr,
    Stmt, TernaryExpr, Type, TypePath, UnaryExpr, UnaryOp,
};
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::symbols;
use crate::semantic::type_checker::LambdaCapture;
use crate::semantic::type_checker::types::await_output_type;

use super::typed_ast::{
    HirCapture, HirClass, HirDeferBody, HirDeferId, HirExpr, HirExprKind, HirFunction, HirMatchArm,
    HirParam, HirPattern, HirProgram, HirSelectCase, HirSelectCaseKind, HirStmt,
};

/// Complete semantic input to production lowering. Sparse maps represent
/// optional per-node facts, never an optional checking phase. The references
/// borrow one checked unit; no fact maps are cloned at this boundary.
pub struct SemanticFacts<'a> {
    pub symbols: &'a symbols::SymbolTable,
    pub expr_types: &'a HashMap<ExprId, Type>,
    pub enum_variant_resolutions: &'a HashMap<ExprId, String>,
    pub pattern_resolutions: &'a HashMap<PatternId, crate::parser::ast::Pattern>,
    pub arm_leaves: &'a HashMap<crate::parser::ast::BodyId, bool>,
    pub normalized_types: &'a HashMap<Type, Type>,
    pub static_call_classes: &'a HashMap<ExprId, crate::semantic::ids::TypeId>,
    pub lambda_captures: &'a HashMap<ExprId, Vec<LambdaCapture>>,
}

impl<'a> SemanticFacts<'a> {
    pub fn from_checker(checker: &'a crate::semantic::TypeChecker) -> Self {
        Self {
            symbols: &checker.symbols,
            expr_types: &checker.expr_types,
            enum_variant_resolutions: &checker.enum_variant_resolutions,
            pattern_resolutions: &checker.pattern_resolutions,
            arm_leaves: &checker.arm_leaves,
            normalized_types: &checker.normalized_types,
            static_call_classes: &checker.static_call_classes,
            lambda_captures: &checker.lambda_captures,
        }
    }

    fn lowering(&self) -> PartialCheckerTables<'_> {
        PartialCheckerTables {
            checked: true,
            mutable_capture_declarations: Default::default(),
            symbols: Some(self.symbols),
            expr_types: Some(self.expr_types),
            enums: Some(&self.symbols.enums),
            enum_variant_resolutions: Some(self.enum_variant_resolutions),
            pattern_resolutions: Some(self.pattern_resolutions),
            arm_leaves: Some(self.arm_leaves),
            normalized_types: Some(self.normalized_types),
            static_call_classes: Some(self.static_call_classes),
            lambda_captures: Some(self.lambda_captures),
        }
    }

    pub(crate) fn normalize(&self, ty: &Type) -> Type {
        self.lowering().normalize(ty)
    }

    pub fn declared_enum_identities(&self, program: &Program) -> HashSet<String> {
        self.lowering().declared_enum_identities(program)
    }

    pub fn codegen_enum_info(
        &self,
        info: &symbols::EnumInfo,
        declared: &HashSet<String>,
    ) -> symbols::EnumInfo<crate::semantic::ids::TypeId> {
        self.lowering().codegen_enum_info(info, declared)
    }
}

pub fn lower_program_with(
    program: &Program,
    facts: &SemanticFacts<'_>,
) -> (HirProgram, Vec<Diagnostic>) {
    lower_program_except(program, facts, &|_| false)
}

pub fn lower_program_except(
    program: &Program,
    facts: &SemanticFacts<'_>,
    foreign: &dyn Fn(&MethodDecl) -> bool,
) -> (HirProgram, Vec<Diagnostic>) {
    lower_program_except_partial(program, &facts.lowering(), foreign)
}

pub fn lower_foreign_methods(
    scope: &Program,
    facts: &SemanticFacts<'_>,
    receivers: &[ForeignClass<'_>],
    classes: &HashMap<crate::semantic::ids::TypeId, super::typed_ast::HirClassInfo>,
) -> (HirProgram, Vec<Diagnostic>) {
    lower_foreign_methods_partial(scope, &facts.lowering(), receivers, classes)
}

pub(crate) fn lower_resolution(
    program: &Program,
    facts: &SemanticFacts<'_>,
) -> super::typed_ast::HirResolution {
    lower_resolution_partial(program, &facts.lowering())
}

/// Explicit partial input for standalone lowering tests and declaration-only
/// queries. Production executable entry points accept [`SemanticFacts`].
/// Internally a complete view shares these accessors, with missing required
/// per-node facts diagnosed instead of using standalone inference.
#[derive(Default)]
pub struct PartialCheckerTables<'a> {
    pub(crate) checked: bool,
    /// One unit-wide index, shared by all lowered functions and nested lambdas.
    pub mutable_capture_declarations: std::sync::OnceLock<std::collections::HashSet<Span>>,
    /// Syntax-only block exit facts computed by checking.
    pub arm_leaves: Option<&'a HashMap<crate::parser::ast::BodyId, bool>>,
    /// Canonical global declarations copied into the owned HIR snapshot.
    pub symbols: Option<&'a symbols::SymbolTable>,
    /// Unqualified enum-variant constructions (`Ok(42)` in an expected-enum
    /// position), keyed by the call's node ID; the value is the resolved enum
    /// name and the variant is the call's callee.
    pub enum_variant_resolutions: Option<&'a HashMap<ExprId, String>>,
    /// Checked reinterpretations of bare match patterns. When this table is
    /// present, an absent entry preserves the parsed binding or downcast;
    /// knowing an enum's identity does not grant access to its bare variants.
    pub pattern_resolutions: Option<&'a HashMap<PatternId, crate::parser::ast::Pattern>>,
    /// Authoritative checked types. Production lowering transcribes these onto
    /// HIR expressions; standalone tests may omit the table.
    pub expr_types: Option<&'a HashMap<ExprId, Type>>,
    /// Every enum the checker registered, under every name it registered it
    /// by: a program's own `Color`, an imported module's `palette::Color`, and
    /// the bare local name a direct type import binds (`Kind` for
    /// `import shapes::Kind;`). The lowering's own sweep only sees THIS unit's
    /// `enum` items, so without this table a `match` on any enum that came
    /// from another file decides its scrutinee is not an enum at all
    /// (imported enum matching case B).
    pub enums: Option<&'a HashMap<crate::semantic::ids::TypeId, symbols::EnumInfo>>,
    /// What the checker made of each type ANNOTATION it normalized, keyed by
    /// the written spelling. Lowering reads annotations off the
    /// AST, where `Arr<i64>` (an aliased import), `std::result::Result<i64, E>`
    /// (a fully qualified std path) and a module's own `Level` (an enum whose
    /// identity is `signal::Level`) are still the words the source used; every
    /// other phase sees the normalized type, so without this the LIR walker
    /// meets a named type nothing has heard of and turns the body down.
    pub normalized_types: Option<&'a HashMap<Type, Type>>,
    /// Resolved class/module name for every checked static expression, including
    /// unchanged spellings. Absence is an error on the production path.
    pub static_call_classes: Option<&'a HashMap<ExprId, crate::semantic::ids::TypeId>>,
    /// What each lambda captures from its enclosing function, keyed by the
    /// lambda's node ID and ordered as the closure environment lays its slots out. Only the
    /// checker knows this: capture is decided by
    /// name resolution, which the AST does not record.
    pub lambda_captures: Option<&'a HashMap<ExprId, Vec<LambdaCapture>>>,
}

#[willow_continuations::methods(normalize)]
impl<'a> PartialCheckerTables<'a> {
    fn cell_declaration(&self, span: &Span) -> bool {
        self.mutable_capture_declarations
            .get_or_init(|| {
                self.lambda_captures
                    .into_iter()
                    .flat_map(|table| table.values())
                    .flatten()
                    .filter(|capture| capture.mutable)
                    .map(|capture| capture.declaration_span)
                    .collect()
            })
            .contains(span)
    }

    /// Re-spell a written annotation as the checker's type. Bottom-up, so a
    /// spelling inside a type argument is rewritten even when the whole type
    /// was never written that way.
    pub(crate) fn normalize(&self, ty: &Type) -> Type {
        self.normalize_declared(ty, &[])
    }

    /// Symbolic declaration parameters must survive until instantiation.
    fn normalize_declared(&self, ty: &Type, bound: &[String]) -> Type {
        if let Type::Named(name) = ty
            && bound.contains(name)
        {
            return ty.clone();
        }
        if let Type::Named(name) = ty
            && let Some(spelled) = self.receiver_spelling(name)
        {
            return Type::Named(spelled);
        }
        let ty = self.normalized_types.and_then(|m| m.get(ty)).unwrap_or(ty);
        let rebuilt = match ty {
            Type::Named(name) => self
                .enums
                .and_then(|enums| enums.get(&crate::semantic::ids::TypeId::from_source_name(name)))
                .map(|info| Type::Named(info.name.clone()))
                .or_else(|| {
                    self.symbols
                        .and_then(|symbols| symbols.lookup_class(name))
                        .map(|info| Type::Named(info.name.clone()))
                })
                .or_else(|| {
                    self.symbols
                        .and_then(|symbols| symbols.lookup_interface(name))
                        .map(|info| Type::Named(info.name.clone()))
                })
                .unwrap_or_else(|| ty.clone()),
            Type::Array(elem) | Type::FixedArray(elem, _) => {
                Type::Array(Box::new(self.normalize_declared(elem, bound)))
            }
            Type::Generic(name, args) => Type::Generic(
                self.symbols
                    .and_then(|symbols| symbols.lookup_interface(name))
                    .map(|info| info.name.clone())
                    .or_else(|| {
                        self.enums
                            .and_then(|enums| {
                                enums.get(&crate::semantic::ids::TypeId::from_source_name(name))
                            })
                            .map(|info| info.name.clone())
                    })
                    .unwrap_or_else(|| name.clone()),
                args.iter()
                    .map(|a| self.normalize_declared(a, bound))
                    .collect(),
            ),
            Type::Fn(params, ret) => Type::Fn(
                params
                    .iter()
                    .map(|p| self.normalize_declared(p, bound))
                    .collect(),
                Box::new(self.normalize_declared(ret, bound)),
            ),
            Type::Closure(params, ret) => Type::Closure(
                params
                    .iter()
                    .map(|p| self.normalize_declared(p, bound))
                    .collect(),
                Box::new(self.normalize_declared(ret, bound)),
            ),
            other => other.clone(),
        };
        match self.normalized_types.and_then(|m| m.get(&rebuilt)) {
            Some(normalized) => normalized.clone(),
            None => rebuilt,
        }
    }

    /// How this scope must spell the type `name` is an alias of, when that
    /// is the entry file's bare identity and this scope's own types shadow
    /// it: a generic instance's argument can be the entry's `Me` in a unit
    /// that declares its own. `None` leaves `name` to the
    /// ordinary normalization.
    fn receiver_spelling(&self, name: &str) -> Option<String> {
        // The kind too: a receiver class and the scope's own interface can
        // share one bare identity.
        let identity = |name: &str| {
            let symbols = self.symbols?;
            (self.enum_identity(name).map(|id| (id, 0)))
                .or_else(|| {
                    symbols
                        .lookup_class(name)
                        .map(|info| (info.name.as_str(), 1))
                })
                .or_else(|| {
                    symbols
                        .lookup_interface(name)
                        .map(|info| (info.name.as_str(), 2))
                })
        };
        let resolved = identity(name)?;
        let target = resolved.0;
        if target == name || QualifiedName::new(&target).is_qualified() || target.starts_with('$') {
            return None;
        }
        (identity(target)? != resolved).then(|| crate::semantic::ids::foreign_receiver_name(target))
    }

    /// The identity this checker gave the enum written `name` here.
    fn enum_identity(&self, name: &str) -> Option<&str> {
        self.enums?
            .get(&crate::semantic::ids::TypeId::from_source_name(name))
            .map(|info| info.name.as_str())
    }

    /// The identities of the enums `program` itself declares.
    pub fn declared_enum_identities(&self, program: &Program) -> HashSet<String> {
        program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Enum(e) => self.enum_identity(&e.name).map(str::to_owned),
                _ => None,
            })
            .collect()
    }

    /// `info` as the back-end's build-wide enum table must hold it: every
    /// payload spelled by its identity. The checker keeps a declaring unit's
    /// payloads in that unit's own spelling — `Box2` under `import bx::Bx as
    /// Box2;`, `Shape` for an item-imported interface — which no other unit
    /// can resolve, so an importer that met the enum through this metadata
    /// turned every function taking it down. Only an enum in
    /// `declared` (see [`Self::declared_enum_identities`]) is rewritten: an
    /// imported one is already canonical, and this unit's spellings say
    /// nothing about another module's names.
    pub fn codegen_enum_info(
        &self,
        info: &symbols::EnumInfo,
        declared: &HashSet<String>,
    ) -> symbols::EnumInfo<crate::semantic::ids::TypeId> {
        if !declared.contains(&info.name) {
            return info.to_semantic();
        }
        let mut canonical = info.clone();
        for variant in &mut canonical.variants {
            for ty in &mut variant.payload_types {
                *ty = self.normalize_declared(ty, &info.type_params);
            }
        }
        canonical.to_semantic()
    }

    /// A lambda's full inferred callable type, from the checker's expression
    /// table — including the parameter types a call site supplied, which the
    /// AST cannot spell. Either callable form answers: which one it is says
    /// whether the lambda captures, not what its signature is.
    fn lambda_fn_type(&self, id: &ExprId) -> Option<&Type> {
        match self.expr_types.and_then(|m| m.get(id)) {
            Some(ty @ (Type::Fn(..) | Type::Closure(..))) => Some(ty),
            _ => None,
        }
    }

    /// What the lambda at `span` captures, in environment-slot order.
    fn lambda_captures(&self, id: &ExprId) -> &[LambdaCapture] {
        self.lambda_captures
            .and_then(|m| m.get(id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The value of the module-level `const` a bare name resolves to in this
    /// unit (its own, or one an item import bound), constant resolution.
    fn constant(&self, name: &str) -> Option<&crate::parser::ast::ConstValue> {
        self.symbols?.lookup_func(name)?.constant.as_ref()
    }

    /// The value of `module::NAME` when it names a module-level `const`.
    fn module_constant(&self, module: &str, name: &str) -> Option<&crate::parser::ast::ConstValue> {
        self.symbols?
            .lookup_module_func(module, name)?
            .constant
            .as_ref()
    }

    fn enum_variant_resolution(&self, id: &ExprId) -> Option<&String> {
        self.enum_variant_resolutions.and_then(|m| m.get(id))
    }

    /// The class a static call actually names, given the written spelling.
    fn static_call_class(&self, id: &ExprId, written: &str) -> String {
        self.static_call_classes
            .and_then(|m| m.get(id))
            .map(ToString::to_string)
            .unwrap_or_else(|| written.to_string())
    }

    fn expr_type(&self, id: &ExprId) -> Option<Type> {
        self.expr_types
            .and_then(|m| m.get(id))
            .map(Type::array_storage_type)
    }

    /// The checker's enums, re-keyed by the name they are WRITTEN with —
    /// `TypeId`'s own rendering, so `palette::Color` stays qualified and a
    /// direct type import's local `Kind` stays bare. Those are exactly the
    /// names a scrutinee's `Type::Named` carries (imported enum matching case B).
    fn declared_enums(&self) -> impl Iterator<Item = (String, EnumInfo)> + '_ {
        self.enums.into_iter().flatten().map(|(id, info)| {
            (
                id.to_string(),
                EnumInfo {
                    type_params: info.type_params.clone(),
                    variants: info
                        .variants
                        .iter()
                        .map(|v| {
                            (
                                v.name.clone(),
                                v.payload_types
                                    .iter()
                                    .map(Type::array_storage_type)
                                    .collect(),
                            )
                        })
                        .collect(),
                },
            )
        })
    }
}

/// Lower a whole program's free functions to typed HIR. Non-function items and
/// constructs outside slice 1 are reported as diagnostics; the functions that
/// do lower cleanly are still returned, so callers can make progress.
pub fn lower_program(program: &Program) -> (HirProgram, Vec<Diagnostic>) {
    lower_program_with_partial(program, &PartialCheckerTables::default())
}

/// Like [`lower_program`], additionally consulting the type checker's side
/// tables for information the structural lowering cannot derive.
pub fn lower_program_with_partial(
    program: &Program,
    tables: &PartialCheckerTables,
) -> (HirProgram, Vec<Diagnostic>) {
    lower_program_except_partial(program, tables, &|_| false)
}

/// [`lower_program_with_partial`], leaving out the class methods `foreign` selects:
/// interface defaults another unit declared, which [`lower_foreign_methods_partial`]
/// lowers in that unit's scope instead.
fn lower_program_except_partial(
    program: &Program,
    tables: &PartialCheckerTables,
    foreign: &dyn Fn(&MethodDecl) -> bool,
) -> (HirProgram, Vec<Diagnostic>) {
    let LowerEnv {
        fn_returns,
        classes,
        enums,
    } = lower_env(program, tables);
    let mut functions = Vec::new();
    let mut hir_classes = Vec::new();
    let mut diagnostics = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) if f.rust_bridge.is_some() => {}
            Item::Function(f) => match lower_function(f, &fn_returns, &classes, &enums, tables) {
                Ok(func) => functions.push(func),
                Err(d) => diagnostics.push(d),
            },
            Item::Class(c) => {
                let mut methods = Vec::new();
                for ctor in &c.constructors {
                    match lower_constructor(ctor, &c.name, &fn_returns, &classes, &enums, tables) {
                        Ok(func) => methods.push(func),
                        Err(d) => diagnostics.push(d),
                    }
                }
                for m in c.methods.iter().filter(|m| !foreign(m)) {
                    match lower_method(m, &c.name, None, &fn_returns, &classes, &enums, tables) {
                        Ok(func) => methods.push(func),
                        Err(d) => diagnostics.push(d),
                    }
                }
                for field in c.fields.iter().filter(|field| field.is_static) {
                    if let Some(init) = &field.initializer {
                        let mut ctx = LowerCtx::new(&fn_returns, &classes, &enums, tables);
                        ctx.current_class = Some(c.name.clone());
                        match lower_expr(init, &mut ctx) {
                            Ok(mut value) => {
                                let return_type = ctx.normalize(&field.ty);
                                retype_array_literal(&mut value, &return_type);
                                functions.push(HirFunction {
                                    requires: Vec::new(),
                                    ensures: Vec::new(),
                                    name: crate::semantic::ids::FunctionId::method(
                                        crate::semantic::ids::TypeId::from_source_name(&c.name),
                                        format!("$static_init.{}", field.name),
                                    ),
                                    is_async: false,
                                    params: Vec::new(),
                                    return_type: return_type.into(),
                                    body: vec![HirStmt::Return {
                                        value: Some(value),
                                        span: init.span(),
                                    }],
                                    span: init.span(),
                                });
                            }
                            Err(diagnostic) => diagnostics.push(diagnostic),
                        }
                    }
                }
                hir_classes.push(HirClass {
                    name: c.name.clone().into(),
                    methods,
                    span: c.span,
                });
            }
            _ => {}
        }
    }
    (
        HirProgram {
            functions,
            classes: hir_classes,
            resolution: lower_resolution_partial(program, tables),
        },
        diagnostics,
    )
}

/// Lower the interface defaults that classes of other units received from
/// `scope`'s interfaces in that unit's scope: `scope` and
/// `scope_tables` are the declaring unit's program and checked tables, whose
/// typed record of each default is what the copies reuse. The scope's
/// signatures and resolution are built once for all `receivers`; `classes`
/// holds their class chains, taken from the implementing units.
fn lower_foreign_methods_partial(
    scope: &Program,
    scope_tables: &PartialCheckerTables,
    receivers: &[ForeignClass<'_>],
    classes: &HashMap<crate::semantic::ids::TypeId, super::typed_ast::HirClassInfo>,
) -> (HirProgram, Vec<Diagnostic>) {
    let LowerEnv {
        fn_returns,
        classes: mut env_classes,
        enums,
    } = lower_env(scope, scope_tables);
    // A call on `self` must take the receiver's own member types: the
    // checker typed the body against the interface, so a `Self` result
    // there names the interface rather than the receiver class.
    for (id, info) in classes {
        let std::collections::hash_map::Entry::Vacant(entry) =
            env_classes.map.entry(id.to_string())
        else {
            continue;
        };
        let entry = entry.insert(ClassInfo::default());
        entry.base = info.base.map(|base| base.to_string());
        for (name, ty) in &info.fields {
            entry.fields.insert(name.clone(), ty.to_source());
        }
        for (name, ty) in &info.static_fields {
            entry.static_fields.insert(name.clone(), ty.to_source());
        }
        for (name, signature) in &info.methods {
            let ty = call_site_type(&signature.return_type.to_source(), signature.is_async);
            let members = if signature.is_static {
                &mut entry.static_methods
            } else {
                &mut entry.methods
            };
            members.insert(name.clone(), ty);
        }
    }
    let mut hir_classes = Vec::with_capacity(receivers.len());
    let mut diagnostics = Vec::new();
    for receiver in receivers {
        let self_ty = Type::Named(receiver.key.to_owned());
        let mut lowered = Vec::with_capacity(receiver.methods.len());
        for &(_, m) in &receiver.methods {
            match lower_method(
                m,
                receiver.key,
                Some(&self_ty),
                &fn_returns,
                &env_classes,
                &enums,
                scope_tables,
            ) {
                Ok(func) => lowered.push(func),
                Err(d) => diagnostics.push(d),
            }
        }
        hir_classes.push(HirClass {
            name: receiver.key.to_owned().into(),
            methods: lowered,
            span: receiver.span,
        });
    }
    let mut resolution = lower_resolution_partial(scope, scope_tables);
    // The receivers' class chains are their implementing units' declarations;
    // module class identities are qualified, and a bare one the scope's own
    // names shadow arrives under `FOREIGN_RECEIVER_PREFIX`, so
    // none collides with the scope's.
    for (id, info) in classes {
        resolution
            .classes
            .entry(*id)
            .or_insert_with(|| info.clone());
    }
    (
        HirProgram {
            functions: Vec::new(),
            classes: hir_classes,
            resolution,
        },
        diagnostics,
    )
}

/// A class that copied interface defaults from another unit, with each copy's
/// body identity and the declaration it instantiates.
pub struct ForeignClass<'a> {
    /// The receiver's class identity, as its own unit's tables normalize it,
    /// or under `FOREIGN_RECEIVER_PREFIX` when that is a bare name the scope's
    /// own items shadow.
    pub key: &'a str,
    pub span: Span,
    pub methods: Vec<(crate::parser::ast::BodyId, &'a MethodDecl)>,
}

/// The signatures one unit's own declarations give its bodies.
struct LowerEnv {
    fn_returns: HashMap<String, Type>,
    classes: Classes,
    enums: Enums,
}

fn lower_env(program: &Program, tables: &PartialCheckerTables) -> LowerEnv {
    // Builtin functions the checker registers (register_builtin_functions):
    // their call-site types, so calls to them lower like any other call.
    let mut fn_returns: HashMap<String, Type> = HashMap::from([
        ("pow".to_string(), Type::F64),
        ("powf".to_string(), Type::F64),
        ("gc_collect".to_string(), Type::Void),
        ("gc_minor_collect".to_string(), Type::Void),
        ("gc_task_frame_diagnostics".to_string(), Type::Void),
        ("sched_local_pop_hits".to_string(), Type::I64),
        ("sched_global_pop_hits".to_string(), Type::I64),
        ("sched_global_pop_attempts".to_string(), Type::I64),
        ("sched_steal_attempts".to_string(), Type::I64),
        ("sched_steal_successes".to_string(), Type::I64),
        ("sched_steal_failures".to_string(), Type::I64),
        ("sched_victim_locks".to_string(), Type::I64),
        ("sched_global_pushes".to_string(), Type::I64),
        ("sched_local_pushes".to_string(), Type::I64),
        ("gc_allocated_bytes".to_string(), Type::I64),
        ("gc_total_allocated_bytes".to_string(), Type::I64),
        ("gc_tlab_fast_allocations".to_string(), Type::I64),
        ("gc_tlab_slow_allocations".to_string(), Type::I64),
        ("gc_tlab_refills".to_string(), Type::I64),
        ("gc_tlab_large_allocations".to_string(), Type::I64),
        ("gc_tlab_reserved_bytes".to_string(), Type::I64),
        ("gc_minor_collections".to_string(), Type::I64),
        ("gc_promoted_objects".to_string(), Type::I64),
        ("gc_moved_objects".to_string(), Type::I64),
        ("gc_pause_count".to_string(), Type::I64),
        ("gc_pause_total_ns".to_string(), Type::I64),
        ("gc_pause_max_ns".to_string(), Type::I64),
        ("gc_survivor_copies".to_string(), Type::I64),
        ("gc_survivor_bytes".to_string(), Type::I64),
        ("gc_tenured_objects".to_string(), Type::I64),
        ("gc_tenured_bytes".to_string(), Type::I64),
        ("gc_pinned_promotions".to_string(), Type::I64),
        ("gc_survivor_space_reserved".to_string(), Type::I64),
        ("gc_survivor_space_live".to_string(), Type::I64),
        ("gc_remembered_set_size".to_string(), Type::I64),
        ("gc_dirty_card_count".to_string(), Type::I64),
        ("gc_write_barrier_hits".to_string(), Type::I64),
        ("gc_old_region_count".to_string(), Type::I64),
        ("gc_old_region_reserved_bytes".to_string(), Type::I64),
        ("gc_old_region_live_bytes".to_string(), Type::I64),
        ("gc_old_region_fragmentation_bytes".to_string(), Type::I64),
        ("gc_large_object_region_count".to_string(), Type::I64),
        ("gc_pinned_region_count".to_string(), Type::I64),
        ("gc_old_region_allocations".to_string(), Type::I64),
        ("gc_old_region_reuses".to_string(), Type::I64),
        ("gc_old_regions_released".to_string(), Type::I64),
        ("gc_major_collections".to_string(), Type::I64),
        ("panic".to_string(), Type::Never),
        (
            "recover".to_string(),
            B::Option.apply(vec![Type::Named("PanicInfo".to_string())]),
        ),
        ("format".to_string(), Type::String),
        (
            "sleep".to_string(),
            Type::Generic("Future".to_string(), vec![Type::Void]),
        ),
        (
            "yield".to_string(),
            Type::Generic("Future".to_string(), vec![Type::Void]),
        ),
    ]);
    // `PanicInfo` first: a program cannot redeclare it (the checker refuses),
    // so nothing here overwrites it.
    let mut classes = Classes::with_runtime_types();
    let mut enums = Enums::with_prelude();
    // Before this unit's own items, so a locally declared enum still shadows
    // an imported one of the same name below (imported enum matching case B).
    for (name, info) in tables.declared_enums() {
        enums.map.insert(name, info);
    }
    for item in &program.items {
        match item {
            Item::Function(f) => {
                fn_returns.insert(
                    f.name.clone(),
                    call_site_type(&tables.normalize(&f.return_type), f.is_async),
                );
            }
            Item::Class(c) => {
                // The base keeps its module qualifier: a local `Sized extends
                // shapes::Sized` must not read as `Sized extends Sized`, which
                // sent `Classes::resolve` around a one-class loop forever. A qualified base is
                // not in this map (it holds
                // only the unit's own classes), so the walk stops there and the
                // member type comes from the checker tables instead.
                let mut info = ClassInfo {
                    base: c.base_class.as_ref().map(Classes::base_key),
                    ..ClassInfo::default()
                };
                for f in &c.fields {
                    if f.is_static {
                        info.static_fields
                            .insert(f.name.clone(), tables.normalize(&f.ty));
                    } else {
                        info.fields.insert(f.name.clone(), tables.normalize(&f.ty));
                    }
                }
                for m in &c.methods {
                    let call_ty = call_site_type(&tables.normalize(&m.return_type), m.is_async);
                    if m.is_static {
                        info.static_methods.insert(m.name.clone(), call_ty);
                    } else {
                        info.methods.insert(m.name.clone(), call_ty);
                    }
                }
                classes.map.insert(c.name.clone(), info);
            }
            // After the prelude's own, so a program enum of the same name
            // shadows it — the order `register_prelude` gives the checker.
            //
            // Payload types are canonicalized like field and parameter types:
            // a payload class imported from another module (`Leaf(Bx)` after
            // `import bx::Bx;`) must bind as `bx::Bx`, the type every member
            // access on the binding resolves against. The checker keeps this
            // unit's spelling, so its entry under the enum's identity — the
            // name a module's own scrutinee carries — is replaced as well. Imported enums are
            // already canonical.
            Item::Enum(e) => {
                let mut info = enum_info(e);
                for payload in info.variants.values_mut() {
                    for ty in payload {
                        *ty = tables.normalize_declared(ty, &e.type_params);
                    }
                }
                if let Some(identity) = tables.enum_identity(&e.name)
                    && identity != e.name
                {
                    enums.map.insert(identity.to_owned(), info.clone());
                }
                enums.map.insert(e.name.clone(), info);
            }
            _ => {}
        }
    }

    LowerEnv {
        fn_returns,
        classes,
        enums,
    }
}

/// The type a CALL to a function/method produces at the call site: an async
/// fn's call captures its arguments into a `Task<T>`; `await` unwraps it.
fn call_site_type(return_type: &Type, is_async: bool) -> Type {
    if is_async {
        Type::Generic("Task".to_string(), vec![return_type.clone()])
    } else {
        return_type.clone()
    }
}

/// One enum's declared shape: its type parameters and each variant's payload
/// types (with type parameters still symbolic, substituted per use site).
#[derive(Default, Clone)]
struct EnumInfo {
    type_params: Vec<String>,
    variants: HashMap<String, Vec<Type>>,
}

/// All enums in the program plus the prelude's, used to type variant
/// constructions and to bind `match` pattern payloads.
#[derive(Default)]
struct Enums {
    map: HashMap<String, EnumInfo>,
}

/// The prelude's enums, read from [`crate::prelude::PRELUDE_SOURCE`] itself.
///
/// Parsed rather than transcribed: the list used to be a
/// hand-written `Option` + `Result` pair, so `IoError`, `ParseFloatError` and
/// `Cancelled` had no entry here. An unqualified pattern on one of them —
/// `Failed(msg)` on an `IoError` — then missed the variant lookup in
/// [`lower_match`] and lowered as an interface downcast onto a class named
/// `Failed`, a type that does not exist. Reading the prelude keeps the two in
/// step for every enum it declares now and every one it gains later.
fn prelude_enums() -> &'static HashMap<String, EnumInfo> {
    static PRELUDE: std::sync::OnceLock<HashMap<String, EnumInfo>> = std::sync::OnceLock::new();
    PRELUDE.get_or_init(|| {
        let tokens = crate::lexer::Lexer::new(crate::prelude::PRELUDE_SOURCE)
            .tokenize()
            .expect("the prelude lexes");
        let (prelude, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "the prelude parses: {errors:?}");
        prelude
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Enum(e) => Some((e.name.clone(), enum_info(e))),
                _ => None,
            })
            .collect()
    })
}

/// One enum declaration's lowering-side shape.
fn enum_info(e: &crate::parser::ast::EnumDecl) -> EnumInfo {
    EnumInfo {
        type_params: e.type_params.clone(),
        variants: e
            .variants
            .iter()
            .map(|v| (v.name.clone(), v.payload.clone()))
            .collect(),
    }
}

impl Enums {
    fn with_prelude() -> Self {
        Self {
            map: prelude_enums().clone(),
        }
    }
}

/// Substitute symbolic type parameters (`Named(param)`) with concrete types.
fn subst_type(ty: &Type, subst: &HashMap<String, Type>) -> Type {
    ty.substitute_names(|name| subst.get(name).cloned())
}

/// Type information for one class, collected from its declaration.
#[derive(Default)]
struct ClassInfo {
    fields: HashMap<String, Type>,
    methods: HashMap<String, Type>,
    static_fields: HashMap<String, Type>,
    static_methods: HashMap<String, Type>,
    base: Option<String>,
}

/// All classes in the program, used to type field/method access, `new`, static
/// reads/calls, and to resolve inherited members along the base-class chain.
#[derive(Default)]
struct Classes {
    map: HashMap<String, ClassInfo>,
}

impl Classes {
    /// The classes no source file declares. `PanicInfo` is the only one: the
    /// checker defines it, the runtime is the only thing that builds one, and a
    /// `recover()` handler reads its fields like any other object's. Without it
    /// here, `info.line` found no field on its receiver and the enclosing
    /// function historically used the now-retired AST emitter.
    fn with_runtime_types() -> Self {
        let panic_info = ClassInfo {
            fields: builtin_types::panic_info_fields()
                .into_iter()
                .map(|(name, ty)| (name.to_string(), ty))
                .collect(),
            ..ClassInfo::default()
        };
        Self {
            map: HashMap::from([("PanicInfo".to_string(), panic_info)]),
        }
    }

    /// The key a class's `extends` target has in this table: a local name as
    /// written, a qualified path with its module kept.
    fn base_key(base: &TypePath) -> String {
        base.spelling()
    }

    /// Walk the base-class chain from `class`, returning the first member type
    /// `pick` finds. Stops if a base class is not in the program (e.g. external)
    /// or if the chain revisits a class: the checker rejects an `extends` cycle
    /// (E0426), so lowering never sees one, but this walk must still terminate
    /// on any table it is handed.
    fn resolve<F: Fn(&ClassInfo) -> Option<Type>>(&self, class: &str, pick: F) -> Option<Type> {
        let mut current = Some(class);
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name) {
                return None;
            }
            let info = self.map.get(name)?;
            if let Some(ty) = pick(info) {
                return Some(ty);
            }
            current = info.base.as_deref();
        }
        None
    }

    fn field_type(&self, class: &str, field: &str) -> Option<Type> {
        self.resolve(class, |info| info.fields.get(field).cloned())
    }

    fn method_type(&self, class: &str, method: &str) -> Option<Type> {
        self.resolve(class, |info| info.methods.get(method).cloned())
    }

    fn static_field_type(&self, class: &str, field: &str) -> Option<Type> {
        self.resolve(class, |info| info.static_fields.get(field).cloned())
    }

    fn static_method_type(&self, class: &str, method: &str) -> Option<Type> {
        self.resolve(class, |info| info.static_methods.get(method).cloned())
    }
}

/// Lower a single free function against the program's function signatures.
fn lower_function(
    f: &FunctionDecl,
    fn_returns: &HashMap<String, Type>,
    classes: &Classes,
    enums: &Enums,
    tables: &PartialCheckerTables,
) -> Result<HirFunction, Diagnostic> {
    let mut ctx = LowerCtx::new(fn_returns, classes, enums, tables);
    let mut params = Vec::with_capacity(f.params.len());
    for p in &f.params {
        let ty = ctx.normalize(&p.ty);
        let name = ctx.bind(p.name.clone(), ty.clone());
        params.push(HirParam {
            name,
            ty: ty.into(),
            by_reference: !matches!(p.mode, crate::parser::ast::ParamMode::Value),
            span: p.span,
        });
    }
    let return_type = ctx.normalize(&f.return_type);
    let requires = f
        .requires
        .iter()
        .map(|e| lower_expr(e, &mut ctx).map(contract_expression))
        .collect::<Result<Vec<_>, _>>()?;
    let ensures = if f.ensures.is_empty() {
        Vec::new()
    } else {
        ctx.push_scope();
        ctx.bind("result".into(), return_type.clone());
        let clauses = f
            .ensures
            .iter()
            .map(|e| lower_expr(e, &mut ctx).map(contract_expression))
            .collect::<Result<Vec<_>, _>>()?;
        ctx.pop_scope();
        clauses
    };
    let body = lower_block(&f.body, &mut ctx)?;
    Ok(HirFunction {
        requires,
        ensures,
        name: f.name.clone().into(),
        is_async: f.is_async,
        params,
        return_type: return_type.into(),
        body,
        span: f.span,
    })
}

/// Lower a class method. An instance method's receiver is bound as `self` (typed
/// as the class) so `self.field` / `self.method()` in the body resolve against
/// the class registry; static methods have no receiver.
fn lower_method(
    m: &MethodDecl,
    class_name: &str,
    self_ty: Option<&Type>,
    fn_returns: &HashMap<String, Type>,
    classes: &Classes,
    enums: &Enums,
    tables: &PartialCheckerTables,
) -> Result<HirFunction, Diagnostic> {
    let mut ctx = LowerCtx::new(fn_returns, classes, enums, tables);
    ctx.current_class = Some(class_name.to_owned());
    let mut params = Vec::with_capacity(m.params.len() + 1);
    // Explicit and implicit `self` spellings normalize to the same receiver.
    // A foreign default's receiver is already the class identity: this
    // unit's tables would read its local spelling as another class.
    if !m.is_static {
        let self_ty = self_ty
            .cloned()
            .unwrap_or_else(|| ctx.normalize(&Type::Named(class_name.to_string())));
        ctx.bind("self".to_string(), self_ty.clone());
        if m.is_interface_default {
            ctx.receiver = Some(self_ty.clone());
        }
        params.push(HirParam {
            name: "self".to_string(),
            ty: self_ty.into(),
            by_reference: false,
            span: m.span,
        });
    }
    for p in &m.params {
        let ty = ctx.normalize(&p.ty);
        let name = ctx.bind(p.name.clone(), ty.clone());
        params.push(HirParam {
            name,
            ty: ty.into(),
            by_reference: !matches!(p.mode, crate::parser::ast::ParamMode::Value),
            span: p.span,
        });
    }
    let return_type = ctx.normalize(&m.return_type);
    let body = lower_block(&m.body, &mut ctx)?;
    Ok(HirFunction {
        requires: Vec::new(),
        ensures: Vec::new(),
        name: m.name.clone().into(),
        is_async: m.is_async,
        params,
        return_type: return_type.into(),
        body,
        span: m.span,
    })
}

/// Lower an `init` constructor as a method named `init` with `self` bound to
/// the class and a `void` return type.
fn lower_constructor(
    ctor: &crate::parser::ast::ConstructorDecl,
    class_name: &str,
    fn_returns: &HashMap<String, Type>,
    classes: &Classes,
    enums: &Enums,
    tables: &PartialCheckerTables,
) -> Result<HirFunction, Diagnostic> {
    let mut ctx = LowerCtx::new(fn_returns, classes, enums, tables);
    ctx.current_class = Some(class_name.to_owned());
    let self_ty = ctx.normalize(&Type::Named(class_name.to_string()));
    ctx.bind("self".to_string(), self_ty.clone());
    let mut params = Vec::with_capacity(ctor.params.len() + 1);
    params.push(HirParam {
        name: "self".to_string(),
        ty: self_ty.into(),
        by_reference: false,
        span: ctor.span,
    });
    for p in &ctor.params {
        let ty = ctx.normalize(&p.ty);
        let name = ctx.bind(p.name.clone(), ty.clone());
        params.push(HirParam {
            name,
            ty: ty.into(),
            by_reference: !matches!(p.mode, crate::parser::ast::ParamMode::Value),
            span: p.span,
        });
    }
    let body = lower_block(&ctor.body, &mut ctx)?;
    Ok(HirFunction {
        requires: Vec::new(),
        ensures: Vec::new(),
        name: "init".into(),
        is_async: false,
        params,
        return_type: Type::Void,
        body,
        span: ctor.span,
    })
}

/// One name in scope: the type it was bound with plus the name it carries in
/// the HIR.
#[derive(Clone)]
struct Binding {
    cell: bool,
    ty: Type,
    /// The HIR name. This is the source name for the first binding of that
    /// name in a function, and `name$n` for every later one. LIR has a single flat namespace
    /// per function, so
    /// two source bindings that shadow each other — sibling `for x` loops, a
    /// `let` over a parameter, a `match` arm binding over an outer `let` —
    /// must not arrive there under one name.
    hir_name: String,
}

// A compiler-private, fixed one-element array is the GC-managed cell.
// The source type stays in Binding; only storage and environment slots use Array<T>.
fn cell_storage(binding: &Binding, span: Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::Var(binding.hir_name.clone()),
        ty: Type::Array(Box::new(binding.ty.clone())).into(),
        span,
    }
}

fn cell_index(span: Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::Int(0),
        ty: Type::I64,
        span,
    }
}

fn cell_read(binding: &Binding, span: Span) -> HirExpr {
    HirExpr {
        kind: HirExprKind::Index {
            array: Box::new(cell_storage(binding, span)),
            index: Box::new(cell_index(span)),
        },
        ty: binding.ty.clone().into(),
        span,
    }
}

/// Lowering scope: variables (innermost-last) plus the free-function
/// return types used to type `Call` expressions.
struct LowerCtx<'a> {
    arm_leaves: HashMap<crate::parser::ast::BodyId, bool>,
    scopes: crate::semantic::scopes::Scopes<Binding>,
    /// Scope depth immediately outside the current function/lambda body.
    /// Used to distinguish a body-wide callable shadow from a nested one.
    namespace_scope_base: usize,
    /// Binding suffix counters, also reserving free callees already used.
    /// Not popped with a scope: the suffix has to be unique across the whole
    /// function, not only across the scopes currently open.
    binds_seen: HashMap<String, usize>,
    next_defer_id: u32,
    current_class: Option<String>,
    /// The receiver class an interface default's copy lowers the opaque
    /// `Self` its shared check typed the body with as.
    receiver: Option<Type>,
    fn_returns: &'a HashMap<String, Type>,
    classes: &'a Classes,
    enums: &'a Enums,
    tables: &'a PartialCheckerTables<'a>,
}

impl<'a> LowerCtx<'a> {
    fn new(
        fn_returns: &'a HashMap<String, Type>,
        classes: &'a Classes,
        enums: &'a Enums,
        tables: &'a PartialCheckerTables<'a>,
    ) -> Self {
        Self {
            arm_leaves: HashMap::new(),
            scopes: crate::semantic::scopes::Scopes::with_frame([]),
            namespace_scope_base: 1,
            binds_seen: HashMap::new(),
            next_defer_id: 0,
            current_class: None,
            receiver: None,
            fn_returns,
            classes,
            enums,
            tables,
        }
    }

    /// The checker's type for a written annotation.
    fn normalize(&self, ty: &Type) -> Type {
        self.receiver_self(self.tables.normalize(ty))
    }

    /// The checker's type for expression `id`, with `Self` as the receiver.
    fn expr_type(&self, id: &ExprId) -> Option<Type> {
        self.tables.expr_type(id).map(|ty| self.receiver_self(ty))
    }

    fn receiver_self(&self, ty: Type) -> Type {
        match &self.receiver {
            Some(receiver) => {
                ty.substitute_names(|name| (name == "Self").then(|| receiver.clone()))
            }
            None => ty,
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push();
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Bind `name` in the innermost scope and return the name the HIR uses for
    /// it. Callers MUST build their HIR node from the returned name — it is the
    /// source name only when nothing else in this function has claimed it.
    fn bind(&mut self, name: String, ty: Type) -> String {
        self.bind_cell(name, ty, false)
    }

    fn bind_cell(&mut self, name: String, ty: Type, cell: bool) -> String {
        let seen = self.binds_seen.entry(name.clone()).or_insert(0);
        *seen += 1;
        // A nested local function value and a free function also share the
        // walker's flat lookup namespace. Rename the nested local even on its
        // first binding so leaving that lexical scope exposes the free function
        // again. A function-body binding deliberately keeps the source name:
        // it shadows the free function for the rest of that body.
        let nested_callable_shadow = self.scopes.len() > self.namespace_scope_base + 1
            && self.fn_returns.contains_key(&name);
        let hir_name = if *seen == 1 && !nested_callable_shadow {
            name.clone()
        } else {
            // `$` cannot appear in a source identifier, so a renamed binding
            // can never collide with one the program wrote.
            format!("{name}${seen}")
        };
        self.scopes.insert(
            name,
            Binding {
                cell,
                ty,
                hir_name: hir_name.clone(),
            },
        );
        hir_name
    }

    fn lookup(&self, name: &str) -> Option<&Binding> {
        self.scopes.get(name)
    }

    /// The HIR name of a local binding, or `name` itself when nothing in scope
    /// carries it — a free function, an enum variant, anything lowering
    /// resolves by its source spelling.
    fn hir_name(&self, name: &str) -> String {
        self.lookup(name)
            .map(|b| b.hir_name.clone())
            .unwrap_or_else(|| name.to_string())
    }
}

/// Re-type an array literal (and any array literal nested directly inside it)
/// to the element type its annotated slot declares.
///
/// Checked literals already carry this type. This fallback preserves lowering
/// for callers without checker tables.
///
/// [`TypeChecker::check_array_literal_expecting`]: crate::semantic::type_checker::TypeChecker
fn retype_array_literal(value: &mut HirExpr, target: &Type) {
    let mut pending = vec![(value, target)];
    while let Some((value, target)) = pending.pop() {
        let Type::Array(elem) = target else {
            continue;
        };
        let HirExprKind::Array { elements } = &mut value.kind else {
            continue;
        };
        value.ty = target.into();
        pending.extend(elements.iter_mut().map(|element| (element, &**elem)));
    }
}

#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_block(block: &Block, ctx: &mut LowerCtx) -> Result<Vec<HirStmt>, Diagnostic> {
    ctx.push_scope();
    let mut out = Vec::with_capacity(block.stmts.len());
    for stmt in &block.stmts {
        // Expand only a declaration-proven storage append in statement position.
        // Operand temporaries preserve call evaluation order and managed roots;
        // the length must be read after evaluating the argument.
        if let Stmt::Expr(statement) = stmt
            && let Expr::MethodCall(call) = &statement.expr
            && call.args.len() == 1
            && ctx
                .tables
                .static_call_classes
                .and_then(|owners| owners.get(&call.id))
                .and_then(|owner| {
                    ctx.tables
                        .symbols
                        .and_then(|symbols| symbols.lookup_class(&owner.to_string()))
                })
                .and_then(|class| class.methods.get(&call.method))
                .is_some_and(|method| {
                    method.array_body_optimization
                        == crate::parser::ast::ArrayBodyOptimization::Append
                })
        {
            let object = lower_expr(&call.object, ctx)?;
            let Type::Array(ref element) = object.ty.to_source() else {
                return Err(unsupported(
                    call.span,
                    "storage append requires an array receiver",
                ));
            };
            let value = lower_expr(&call.args[0].expr, ctx)?;
            let counter = ctx.binds_seen.entry("$storage_append".into()).or_default();
            let prefix = format!("$storage_append{}", *counter);
            *counter += 1;
            let receiver_name = format!("{prefix}_receiver");
            let value_name = format!("{prefix}_value");
            let index_name = format!("{prefix}_index");
            let receiver = HirExpr {
                kind: HirExprKind::Var(receiver_name.clone()),
                ty: object.ty.clone(),
                span: call.span,
            };
            let argument = HirExpr {
                kind: HirExprKind::Var(value_name.clone()),
                ty: (**element).clone().into(),
                span: call.span,
            };
            let index = HirExpr {
                kind: HirExprKind::Var(index_name.clone()),
                ty: Type::I64,
                span: call.span,
            };
            out.push(HirStmt::Let {
                name: receiver_name,
                mutable: false,
                ty: object.ty.clone(),
                value: object,
                span: call.span,
            });
            out.push(HirStmt::Let {
                name: value_name,
                mutable: false,
                ty: argument.ty.clone(),
                value,
                span: call.span,
            });
            out.push(HirStmt::Let {
                name: index_name,
                mutable: false,
                ty: Type::I64,
                value: HirExpr {
                    kind: HirExprKind::MethodCall {
                        object: Box::new(receiver.clone()),
                        method: "__abi_len".into(),
                        args: vec![],
                        super_class: None,
                    },
                    ty: Type::I64,
                    span: call.span,
                },
                span: call.span,
            });
            let length = HirExpr {
                kind: HirExprKind::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(index.clone()),
                    rhs: Box::new(HirExpr {
                        kind: HirExprKind::Int(1),
                        ty: Type::I64,
                        span: call.span,
                    }),
                },
                ty: Type::I64,
                span: call.span,
            };
            out.push(HirStmt::Expr(HirExpr {
                kind: HirExprKind::MethodCall {
                    object: Box::new(receiver.clone()),
                    method: "__abi_resize".into(),
                    args: vec![length],
                    super_class: None,
                },
                ty: Type::Void,
                span: call.span,
            }));
            out.push(HirStmt::IndexAssign {
                array: receiver,
                index,
                value: argument,
                span: call.span,
            });
            continue;
        }
        out.push(lower_stmt(stmt, ctx)?);
    }
    ctx.pop_scope();
    Ok(out)
}

#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_stmt(stmt: &Stmt, ctx: &mut LowerCtx) -> Result<HirStmt, Diagnostic> {
    match stmt {
        Stmt::Let(l) => {
            let mut value = lower_expr(&l.init, ctx)?;
            // A `let x: T = ..` annotation pins the binding type; otherwise the
            // type flows from the value expression.
            let binding_ty =
                l.ty.as_ref()
                    .map(|ty| ctx.normalize(ty))
                    .unwrap_or_else(|| (value.ty.clone()).to_source());
            // An annotated array literal takes the ANNOTATION's element type,
            // exactly as `check_array_literal_expecting` typed it. Without this
            // the literal keeps the type of its first element, so
            // `let shapes: Array<Shape> = [new Square(..), new Box2(..)]`
            // reaches HIR as an `Array<Square>` holding a `Box2` — a type the
            // checker never accepted and no consumer can reason from.
            retype_array_literal(&mut value, &binding_ty);
            let cell = ctx.tables.cell_declaration(&l.span);
            let name = ctx.bind_cell(l.name.clone(), binding_ty.clone(), cell);
            let storage_ty = if cell {
                Type::Array(Box::new(binding_ty))
            } else {
                binding_ty
            };
            if cell {
                value = HirExpr {
                    kind: HirExprKind::Array {
                        elements: vec![value],
                    },
                    ty: storage_ty.clone().into(),
                    span: l.span,
                };
            }
            Ok(HirStmt::Let {
                name,
                mutable: l.mutable,
                ty: storage_ty.into(),
                value,
                span: l.span,
            })
        }
        Stmt::Assign(a) => {
            let value = lower_expr(&a.value, ctx)?;
            if let Some(binding) = ctx.lookup(&a.name).filter(|b| b.cell) {
                return Ok(HirStmt::IndexAssign {
                    array: cell_storage(binding, a.span),
                    index: cell_index(a.span),
                    value,
                    span: a.span,
                });
            }
            Ok(HirStmt::Assign {
                name: ctx.hir_name(&a.name),
                value,
                span: a.span,
            })
        }
        Stmt::If(i) => {
            let cond = lower_expr(&i.cond, ctx)?;
            let then_branch = lower_block(&i.then_block, ctx)?;
            let else_branch = match &i.else_block {
                Some(b) => Some(lower_block(b, ctx)?),
                None => None,
            };
            Ok(HirStmt::If {
                cond,
                then_branch,
                else_branch,
                span: i.span,
            })
        }
        Stmt::Break(span) => Ok(HirStmt::Break { span: *span }),
        Stmt::Continue(span) => Ok(HirStmt::Continue { span: *span }),
        // Every source form of `defer` has a HIR shape. The
        // body is lowered where it was written, so a deferred `match
        // recover()` sees the same bindings the registering scope did; a block
        // body gets its own scope, exactly as `lower_block` gives any block.
        Stmt::Defer(d) => {
            let id = HirDeferId(ctx.next_defer_id);
            ctx.next_defer_id = ctx
                .next_defer_id
                .checked_add(1)
                .expect("too many defer sites");
            let body = match &d.body {
                DeferBody::Expr(expr) => HirDeferBody::Expr(lower_expr(expr, ctx)?),
                DeferBody::Block(block) => HirDeferBody::Block(lower_block(block, ctx)?),
            };
            Ok(HirStmt::Defer {
                id,
                body,
                span: d.span,
            })
        }
        // HIR needs explicit acquire/cleanup edges for a critical section, which
        // arrive with the backend lowering. Until then the
        // statement has no HIR shape at all — the type checker rejects it with
        // E2502, so this is unreachable in practice and only keeps the lowering
        // gap explicit.
        Stmt::Lock(l) => {
            let target = lower_expr(&l.target, ctx)?;
            let mut additional = Vec::with_capacity(l.additional.len());
            for member in &l.additional {
                additional.push(super::typed_ast::HirLockTarget {
                    mode: member.mode,
                    target: lower_expr(&member.target, ctx)?,
                    binding: member.binding.clone(),
                    mutable: member.mutable,
                });
            }
            let value_type = |target: &HirExpr| match &target.ty {
                Type::Generic(_, args) if args.len() == 1 => Ok(args[0].clone()),
                _ => Err(unsupported(l.span, "lock target type")),
            };
            ctx.push_scope();
            let binding = ctx.bind(l.binding.clone(), value_type(&target)?.to_source());
            for member in &mut additional {
                member.binding = ctx.bind(
                    member.binding.clone(),
                    value_type(&member.target)?.to_source(),
                );
            }
            let mut body = Vec::with_capacity(l.body.stmts.len());
            for stmt in &l.body.stmts {
                body.push(lower_stmt(stmt, ctx)?);
            }
            ctx.pop_scope();
            Ok(HirStmt::Lock {
                additional,
                mode: l.mode,
                target,
                binding,
                mutable: l.mutable,
                body,
                span: l.span,
            })
        }
        Stmt::While(w) => {
            let cond = lower_expr(&w.cond, ctx)?;
            let body = lower_block(&w.body, ctx)?;
            Ok(HirStmt::While {
                cond,
                body,
                span: w.span,
            })
        }
        Stmt::Return(r) => {
            let value = match &r.value {
                Some(e) => Some(lower_expr(e, ctx)?),
                None => None,
            };
            Ok(HirStmt::Return {
                value,
                span: r.span,
            })
        }
        Stmt::Expr(e) => Ok(HirStmt::Expr(lower_expr(&e.expr, ctx)?)),
        Stmt::FieldAssign(s) => {
            let object = lower_expr(&s.object, ctx)?;
            let value = lower_expr(&s.value, ctx)?;
            Ok(HirStmt::FieldAssign {
                object,
                field: s.field.clone(),
                value,
                span: s.span,
            })
        }
        Stmt::SuperInit(s) => {
            let args = lower_value_args(&s.args, ctx)?;
            Ok(HirStmt::SuperInit { args, span: s.span })
        }
        Stmt::StaticFieldAssign(s) => {
            let value = lower_expr(&s.value, ctx)?;
            let source = if s.class == "Self" {
                ctx.current_class.as_deref().unwrap_or(&s.class)
            } else {
                &s.class
            };
            let class = match ctx.normalize(&Type::Named(source.to_owned())) {
                Type::Named(ref name) => name.clone(),
                _ => source.to_owned(),
            };
            Ok(HirStmt::StaticFieldAssign {
                class: class.into(),
                field: s.field.clone(),
                value,
                span: s.span,
            })
        }
        Stmt::IndexAssign(s) => {
            let array = lower_expr(&s.array, ctx)?;
            let index = lower_expr(&s.index, ctx)?;
            let value = lower_expr(&s.value, ctx)?;
            Ok(HirStmt::IndexAssign {
                array,
                index,
                value,
                span: s.span,
            })
        }
        Stmt::For(s) => {
            let iterable = lower_expr(&s.iterable, ctx)?;
            let iterable =
                if let Some((key, value)) = builtin_types::binary_args(&iterable.ty, B::Map) {
                    HirExpr {
                        ty: Type::Array(Box::new(Type::Generic(
                            "$Tuple2".into(),
                            vec![key.clone(), value.clone()],
                        ))),
                        span: iterable.span,
                        kind: HirExprKind::MethodCall {
                            object: Box::new(iterable),
                            method: "entries".into(),
                            args: vec![],
                            super_class: None,
                        },
                    }
                } else {
                    iterable
                };
            let element_ty = match &iterable.ty {
                Type::Array(inner) => (**inner).clone(),
                ty if builtin_types::unary_arg(ty, B::FrozenArray).is_some() => {
                    builtin_types::unary_arg(ty, B::FrozenArray)
                        .unwrap()
                        .clone()
                }
                // An i64 range yields i64 elements.
                Type::Generic(name, args)
                    if B::from_name(name.name().as_ref()) == Some(B::Range)
                        && args.first() == Some(&Type::I64) =>
                {
                    Type::I64
                }
                _ => {
                    return Err(unsupported(
                        s.span,
                        "for over a non-array, non-range iterable",
                    ));
                }
            };
            ctx.push_scope();
            let name = ctx.bind(s.name.clone(), (element_ty).to_source());
            let body = lower_block(&s.body, ctx)?;
            ctx.pop_scope();
            Ok(HirStmt::For {
                name,
                iterable,
                body,
                span: s.span,
            })
        }
    }
}

/// Lower call/constructor arguments. A reference argument retains its place as
/// a marker expression so eligibility can verify it against the callee mode and
/// codegen can pass its address rather than its value.
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_value_args(args: &[CallArg], ctx: &mut LowerCtx) -> Result<Vec<HirExpr>, Diagnostic> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        let place = lower_expr(&arg.expr, ctx)?;
        match &arg.mode {
            CallArgMode::Value => out.push(place),
            CallArgMode::Reference { .. } => out.push(HirExpr {
                ty: place.ty.clone(),
                span: arg.span,
                kind: HirExprKind::ReferenceArg {
                    place: Box::new(place),
                },
            }),
        }
    }
    Ok(out)
}

/// The class name a value's type names, if it is a (possibly generic) class
/// type — the receiver position for field access and method calls.
fn class_name_of(ty: &Type) -> Option<&str> {
    match ty {
        Type::Named(name) => Some(name),
        Type::Generic(name, _) => Some(name),
        _ => None,
    }
}

/// Return types of the compiler-builtin collection/concurrency methods, keyed
/// by the receiver type (mirrors the checker's method tables).
fn builtin_method_type(receiver: &Type, method: &str) -> Option<Type> {
    match receiver {
        Type::String if method == "len" => Some(Type::I64),
        Type::Array(elem) => match method {
            "__abi_len" => Some(Type::I64),
            "__abi_resize" => Some(Type::Void),
            "__abi_concat" if **elem == Type::String => Some(Type::String),
            "__abi_copy" => Some(B::FrozenArray.apply(vec![(**elem).clone()])),
            _ => None,
        },
        Type::Generic(_, args) => match (
            builtin_types::resolve(receiver)?.id,
            args.as_slice(),
            method,
        ) {
            (B::Map, [k, v], _) => match method {
                "insert" => Some(Type::Void),
                "toString" => Some(Type::String),
                "get" => Some(B::Option.apply(vec![v.clone()])),
                "contains" => Some(Type::Bool),
                "len" => Some(Type::I64),
                "freeze" => Some(B::FrozenMap.apply(vec![k.clone(), v.clone()])),
                _ => None,
            },
            (B::FrozenArray, [_], "len") | (B::FrozenMap, [_, _], "len") => Some(Type::I64),
            (B::FrozenMap, [_, v], "get") => Some(B::Option.apply(vec![v.clone()])),
            (B::FrozenMap, [_, _], "contains") => Some(Type::Bool),
            (B::Task | B::JoinHandle, [_], "cancel") => Some(Type::Void),
            // Cancellation-aware awaitable view of the same frame.
            (B::Task | B::JoinHandle, [t], "result") => Some(B::TaskResult.apply(vec![t.clone()])),
            (B::Task | B::JoinHandle, [_], "is_cancelled") => Some(Type::Bool),
            (B::BlockingCell, [t], "get") => Some(t.clone()),
            (B::BlockingCell, [_], "set") => Some(Type::Void),
            (B::BlockingRwCell, [t], "read") => Some(t.clone()),
            (B::BlockingRwCell, [_], "write") => Some(Type::Void),
            _ => None,
        },
        _ => None,
    }
}

fn binary_result_type(op: &BinOp, lhs_ty: &Type) -> Type {
    match op {
        // Arithmetic preserves the (already type-checked) operand type.
        BinOp::Add
        | BinOp::Sub
        | BinOp::Mul
        | BinOp::Div
        | BinOp::Rem
        | BinOp::Pow
        | BinOp::BitAnd
        | BinOp::BitOr
        | BinOp::BitXor
        | BinOp::Shl
        | BinOp::Shr => lhs_ty.clone(),
        // Comparisons and logical operators always produce `Bool`.
        BinOp::Eq
        | BinOp::Ne
        | BinOp::Lt
        | BinOp::Le
        | BinOp::Gt
        | BinOp::Ge
        | BinOp::And
        | BinOp::Or => Type::Bool,
    }
}

fn lit(kind: HirExprKind, ty: Type, span: Span) -> HirExpr {
    HirExpr {
        kind,
        ty: ty.into(),
        span,
    }
}

fn unsupported(span: Span, what: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        ErrorCode::E0800,
        format!("HIR lowering does not yet support {what}"),
    )
    .with_label(crate::diagnostics::Label::primary(span, "here"))
}

fn missing_fact(span: Span, fact: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        ErrorCode::E0890,
        format!("internal compiler error: missing required semantic fact: {fact}"),
    )
    .with_label(crate::diagnostics::Label::primary(
        span,
        "missing checked fact",
    ))
}

fn internal(span: Span, msg: String) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        ErrorCode::E0800,
        format!("internal compiler error: {msg}"),
    )
    .with_label(crate::diagnostics::Label::primary(span, "here"))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "lower_facts_tests.rs"]
mod facts_tests;

/// Only contract arithmetic uses this internal builtin. It cannot be spelled
/// by source identifiers and remains an effectful call through optimization.
fn contract_expression(mut expression: HirExpr) -> HirExpr {
    expression.visit_mut_preorder(false, |node| {
        use crate::parser::ast::{BinOp, UnaryOp};
        let op = match &node.kind {
            HirExprKind::Binary { op, lhs, .. }
                if lhs.ty == crate::semantic::ids::SemanticType::I64 =>
            {
                match op {
                    BinOp::Add => Some(0),
                    BinOp::Sub => Some(1),
                    BinOp::Mul => Some(2),
                    BinOp::Div => Some(3),
                    BinOp::Rem => Some(4),
                    BinOp::Pow => Some(5),
                    BinOp::Shl => Some(7),
                    BinOp::Shr => Some(8),
                    _ => None,
                }
            }
            HirExprKind::Unary {
                op: UnaryOp::Neg,
                operand,
            } if operand.ty == crate::semantic::ids::SemanticType::I64 => Some(6),
            _ => None,
        };
        if let Some(op) = op {
            let int = |n| HirExpr {
                kind: HirExprKind::Int(n),
                ty: crate::semantic::ids::SemanticType::I64,
                span: node.span,
            };
            let old = std::mem::replace(&mut node.kind, HirExprKind::Int(0));
            let (lhs, rhs) = match old {
                HirExprKind::Binary { lhs, rhs, .. } => (*lhs, *rhs),
                HirExprKind::Unary { operand, .. } => (*operand, int(0)),
                _ => unreachable!(),
            };
            node.kind = HirExprKind::Call {
                callee: "$contract_i64".into(),
                args: vec![int(op), lhs, rhs],
            };
        }
        true
    });
    expression
}
