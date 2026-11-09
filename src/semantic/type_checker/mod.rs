mod contracts;
#[cfg(test)]
mod contracts_tests;
mod effect_contracts;
pub(crate) mod lock_order;
pub(crate) mod purity;
// Conditional advice also covers stored results without tracking value provenance.
const STRING_FIND_SENTINEL_HELP: &str = "if this i64 came from String.find(needle), it is a byte offset, not an Option: -1 means not found; test `index >= 0` before using it, or use `contains(needle)` for a bool";

use crate::qualified_name::QualifiedName;
use crate::semantic::async_borrows::ReferenceArgMode;
pub(crate) mod analysis;
pub(crate) mod body;
mod builtin_methods;
mod check;
mod check_calls;
mod check_collections;
mod check_concurrency;
use builtin_methods::{ArityText, BuiltinArgument, BuiltinArity, constructor_arity, method_arity};
mod check_decls;
mod check_lambda_match;
pub(crate) mod generics;
pub(crate) mod precheck;
pub(crate) mod precheck_journal;
mod signature;
pub use check_lambda_match::LambdaCapture;
mod async_retention;
#[cfg(test)]
mod builtin_contract_tests;
#[cfg(test)]
mod builtin_method_diagnostic_tests;
mod check_ops;
#[cfg(test)]
mod class_cycle_tests;
mod constructor_flow;
mod diagnostics;
mod indirect_lock;
#[cfg(test)]
mod match_guard_tests;
#[cfg(test)]
mod never_tests;
mod resolve;
mod returns;
mod send_sync;
#[cfg(test)]
mod string_find_tests;
#[cfg(test)]
mod subclass_constructor_tests;
#[cfg(test)]
mod super_call_tests;
mod transfer_ownership;
#[cfg(test)]
mod type_arity_tests;
pub(crate) mod types;
pub(crate) use analysis::*;
#[cfg(test)]
use check::check_source;
use diagnostics::*;
use returns::ReturnSite;
pub(crate) use types::*;

use super::symbols::{ClassInfo, FieldInfo, MethodInfo, ParamInfo, StaticPropInfo, SymbolTable};
use crate::compiler_db::effects::{
    DirectLockEffects, LOCK_EFFECT_WAIT, LockEffectCallsite, LockEffectCause, LockEffectKind,
};
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::module::std_registry;
use crate::parser::ast::*;
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::call_graph::ClassHierarchy;
use crate::semantic::concurrency::NonpreemptibleReason;
use crate::semantic::ids::{FunctionId, TypeId};
use crate::stdlib_schema;
use precheck_journal::{Map as ObservedMap, Set as ObservedSet};
use std::collections::{HashMap, HashSet};

/// Unit-level declaration and name-resolution inputs. Body evaluators share
/// this allocation; declaration registration is the only mutating phase.
#[derive(Clone)]
pub struct ResolutionContext {
    pub(crate) array_classes: ObservedMap<Type, String>,
    pub(crate) array_abi_files: ObservedSet<crate::diagnostics::FileId>,
    /// Files of imported modules whose generic templates this unit
    /// instantiates. Their spellings follow the provider's imports, which the
    /// provider's own check validates.
    provider_template_files: ObservedSet<crate::diagnostics::FileId>,
    /// Whether the items being checked belong to the ENTRY program rather than
    /// an imported module. Only [`ReturnSite::EntryMain`] reads it, and only to
    /// decide whether a `main` is the real entry point — a module's `main` is
    /// an ordinary function. `check_program` sets it; a pass that
    /// ever checks module bodies must clear it first.
    pub(crate) entry_program: bool,
    /// The canonical path of the module these items belong to, when they are a
    /// MODULE's. A type declared here is registered under
    /// `module::Name` as well as the bare spelling its own body uses, and
    /// `normalize_type` answers the bare spelling with the qualified identity,
    /// so one enum has ONE name in every unit that can see it. `None` for the
    /// entry program, whose declarations are already unique build-wide.
    pub(crate) module_path: Option<String>,
    /// Build registration spelling used by class layouts and methods.
    pub(crate) class_module_path: Option<String>,
    task_sync_preemption: bool,
    /// Names introduced by imports (module access names and item-import locals),
    /// used to reject local declarations that collide with an import. The span
    /// is the item-import's location, or `None` for module access names.
    imported_names: ObservedMap<String, Option<Span>>,
    /// Canonical declaration routes retained for direct registration APIs.
    /// Driver registrations additionally borrow package-aware per-unit paths.
    module_access_names: ObservedMap<String, String>,
    /// Shared source-import routes built once with the dependency index.
    registration_import_paths: std::rc::Rc<HashMap<String, String>>,
    /// Dependency metadata needed by imported signatures, without a source
    /// import in this unit. Lookups on inferred types still use the metadata;
    /// explicitly written type names must pass `check_source_type_name` first.
    signature_only_modules: ObservedSet<String>,
    /// Collection type names made available by `std::collections` imports.
    imported_collection_types: ObservedSet<String>,
    imported_collection_targets: ObservedSet<String>,
    /// Local aliases for collection types imported from `std::collections`.
    imported_collection_aliases: ObservedMap<String, String>,
    /// Imported std module namespaces, keyed by their local access name.
    imported_std_modules: ObservedMap<String, ImportedStdModule>,
    /// Directly imported synchronous std::fs functions, keyed by their local
    /// alias. These block a native worker and therefore participate in E2604.
    imported_blocking_std_functions: ObservedMap<String, &'static str>,
    /// Enforce the Send/Sync async checks (E2402-E2405). Compiler entry points
    /// always enable these; direct checker tests can isolate individual rules.
    enforce_send_sync: bool,
    /// Whole-program instance-field reassignments. The driver
    /// supplies one summary for every unit; a direct `check_program` derives
    /// it from its own program.
    field_writes: Option<std::rc::Rc<crate::semantic::field_writes::FieldWrites>>,
    /// Synchronous helpers that contain or transitively reach a loop, or that
    /// belong to or reach a recursive call cycle, keyed by `Class::method` (and
    /// bare fn names), with the helper's definition span and the reason. Used
    /// to flag such a method called through a typed NON-`self` receiver from a
    /// task context (E0810) — the AST-only `ConcurrencyAnalyzer` cannot resolve
    /// such a receiver's class.
    nonpreemptible_methods: std::sync::Arc<crate::compiler_db::HelperSummary>,
    /// Non-preemptible methods of IMPORTED classes, keyed by the receiver class
    /// name as the type checker sees it (`module::Class::method` for a
    /// whole-module import, `Local::method` for a direct class import), mapped
    /// to the source module's display name and the reason. Seeded from
    /// `main.rs` before `check_program`. The helper's definition span lives in
    /// another file the entry diagnostic map cannot render, so the E0810 uses a
    /// note instead of a secondary label.
    nonpreemptible_module_methods: HashMap<FunctionId, (String, NonpreemptibleReason)>,
    effect_index: crate::compiler_db::effects::CallableIndex,
}

pub struct TypeChecker {
    /// Contract comparison runs proof checking once with its shared cache.
    pub(crate) defer_static_verification: bool,
    pub(crate) generics: generics::Generics,
    // Declaration-epoch caches: cleared before registering each program.
    marker_contracts: ObservedMap<String, Option<send_sync::Marker>>,
    marker_contract_proven: ObservedSet<(Type, send_sync::Marker)>,

    /// Injected signatures carry provider metadata, not names written in this unit.
    signature_metadata: bool,
    resolution: std::rc::Rc<ResolutionContext>,
    pub(crate) prechecks: Option<std::rc::Rc<precheck::Cache>>,
    pub(crate) declaration_cache: Option<std::rc::Rc<precheck::Cache>>,
    precheck_bodies: HashMap<BodyId, serde_json::Value>,
    precheck_context: serde_json::Value,
    precheck_occurrences: HashMap<String, usize>,
    body_queries: Option<std::rc::Rc<crate::compiler_db::body::BodyQueries>>,
    body_query_error: Option<anyhow::Error>,
    effect_queries: Option<(
        std::rc::Rc<crate::compiler_db::effects::EffectQueries>,
        crate::module::UnitId,
    )>,
    declaration_queries: Option<(
        std::rc::Rc<crate::compiler_db::declarations::DeclarationQueries>,
        crate::module::UnitId,
    )>,
    pub(crate) checked_bodies: Vec<BodyId>,
    /// Injected interface-default copies whose interface-level check this
    /// unit reuses instead of checking them itself.
    pub(crate) reused_bodies: Vec<BodyId>,
    /// Lambdas checked as their own `typed_body` inside the current body.
    lambdas: body::LambdaChildren,
    /// Lambdas that were checked inline because their body query had already
    /// been evaluated; tests assert this stays zero.
    #[cfg(test)]
    pub(crate) inline_lambda_rechecks: usize,
    local: body::BodyState,
    pub symbols: SymbolTable,
    pub errors: Vec<Diagnostic>,
    /// Monotonic error generation, independent of body-output swaps/replay.
    /// Condition recovery must distinguish errors from warnings in O(1).
    error_generation: usize,
    /// Maps the ID of an UNQUALIFIED enum-variant construction (`Ok(42)` in an
    /// expected-enum position) to the enum it resolved to. The backend consults
    /// this to lower such a `Call` as a variant allocation instead of a function
    /// call. The variant name is the call's callee.
    pub enum_variant_resolutions: HashMap<ExprId, String>,
    /// Maps an unqualified match pattern's ID (`Ok(v)` / `Closed`, which parse as
    /// `ClassDowncast` / `Binding`) to the enum-variant pattern it was
    /// reinterpreted as when the scrutinee is an enum. The backend consults this
    /// to lower the arm as a variant match.
    pub pattern_resolutions: HashMap<PatternId, Pattern>,
    /// Syntax-only arm-exit facts; transported with body outputs for HIR reuse.
    pub arm_leaves: HashMap<BodyId, bool>,
    /// The resolved type of every checked expression, keyed by its node ID. The
    /// authoritative record for consumers (HIR lowering) that must not
    /// re-derive types from the AST (typed HIR lowering checker pivot). A node ID is
    /// syntax identity rather than a source coordinate, so a pass that rebuilds
    /// a node cannot silently inherit its neighbour's type.
    pub expr_types: HashMap<ExprId, Type>,
    // One-shot argument results used to infer a source Array static call.
    prechecked_array_args: HashMap<ExprId, Type>,
    /// Reference modes and async retention of resolved calls, keyed by argument identity.
    pub reference_arg_modes: HashMap<ExprId, ReferenceArgMode>,
    /// What every type ANNOTATION the checker normalized became: the written
    /// spelling (`Arr<i64>`, `std::result::Result<i64, String>`, a module's own
    /// `Level`) mapped to the type the rest of the compiler uses for it. HIR lowering takes
    /// annotations straight off the AST,
    /// which is the one place a written spelling could still reach the back end
    /// and be turned down as an unknown type.
    pub normalized_types: HashMap<Type, Type>,
    signature_qualified: HashSet<String>,
    signature_array_before: Option<(bool, bool)>,
    signature_normalized: HashMap<Type, Type>,
    signature_values:
        std::cell::RefCell<HashMap<crate::compiler_db::checks::CheckInput, serde_json::Value>>,
    signature_reads: Option<HashMap<crate::compiler_db::checks::CheckInput, serde_json::Value>>,
    /// Resolved class/module identity for every static call and field, including
    /// unchanged spellings. Interned identities avoid per-site name allocation.
    pub static_call_classes: HashMap<ExprId, crate::semantic::ids::TypeId>,
    /// What each lambda captures from its enclosing function, keyed by the
    /// lambda expression's ID and ordered as the closure environment lays the
    /// slots out. An empty vector denotes a non-capturing
    /// lambda; an absent entry is a missing required fact at lowering.
    pub lambda_captures: HashMap<ExprId, Vec<LambdaCapture>>,
    /// Canonical names of every class already labelled by an E0426 class
    /// `extends` cycle diagnostic. Each member of a cycle discovers the same
    /// cycle from its own declaration; the first one in declaration order
    /// reports it and the rest stay silent.
    reported_class_cycles: ObservedSet<String>,
    invalid_type_names: std::rc::Rc<std::cell::RefCell<ObservedSet<String>>>,
    import_type_hints: std::rc::Rc<std::cell::OnceCell<HashMap<String, Vec<String>>>>,
    /// Collection type names referenced through fully-qualified `std` paths.
    fully_qualified_collection_types: ObservedSet<String>,
    /// Suppress duplicate missing-import diagnostics per type name.
    missing_collection_imports_reported: ObservedSet<String>,
    resolved_calls: HashMap<FunctionId, crate::semantic::call_graph::CallSites>,
    static_reads: HashMap<FunctionId, HashSet<FunctionId>>,
    analysis_calls: HashMap<ExprId, Option<FunctionId>>,
    /// Constructor bodies whose call-agnostic flow reported a missing field;
    /// `finish_effect_analysis` decides their E0842 with panic facts.
    pending_constructor_flow: Vec<crate::parser::ast::BodyId>,
    analysis_symbols: crate::semantic::analysis_symbols::Facts,
    /// Latest `analysis_symbols.references` index of a member use per call
    /// start, so builtin-call classification is O(1) instead of rescanning
    /// argument references. Entries are validated on read because
    /// `analysis_symbols` is swapped per body.
    member_uses: HashMap<(crate::diagnostics::FileId, usize), usize>,
    capture_call_sites: bool,
    /// Source import diagnostics were already emitted before generic expansion.
    pub(crate) source_imports_checked: bool,
    /// Template pre-check before generic expansion: effects are solved and
    /// reported once by the final checking phase.
    pub(crate) template_precheck: bool,
    task_method_calls: Vec<crate::compiler_db::effects::TaskMethodCall>,
    effect_inputs: crate::compiler_db::effects::EffectInputs,
    /// Function values and indirect calls for E2604.
    indirect_lock: crate::semantic::indirect_lock::IndirectInputs,
    effect_graph_only: Option<crate::semantic::call_graph::CallGraph>,
}

/// Internal callable identity for a non-generic interface default body. The
/// `$default$` spelling cannot be written as a Willow method name, so it cannot
/// collide with a source declaration. Keeping this distinct from the interface
/// dispatch union means an unused default (all implementations override it)
/// does not taint the interface method.
fn interface_default_effect_id(interface: &str, method: &str) -> FunctionId {
    FunctionId::method(
        TypeId::from_source_name(interface),
        format!("$default${method}"),
    )
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ImportedStdModule {
    module: String,
    span: Span,
}

struct ReferencePlaceInfo {
    name: String,
    ty: Type,
    mutable: bool,
    is_param: bool,
    declaration_span: Span,
    /// Set when the place is immutable because of what it *is* rather than how
    /// it was declared (a `PanicInfo` field). `&mut` on such a place is
    /// reported with this message instead of the "declare it `mut`" advice,
    /// which would be unactionable.
    immutable_reason: Option<&'static str>,
}

impl std::ops::Deref for TypeChecker {
    type Target = ResolutionContext;
    fn deref(&self) -> &Self::Target {
        &self.resolution
    }
}
impl std::ops::DerefMut for TypeChecker {
    fn deref_mut(&mut self) -> &mut Self::Target {
        std::rc::Rc::make_mut(&mut self.resolution)
    }
}

impl Default for TypeChecker {
    fn default() -> Self {
        Self::new()
    }
}

#[willow_continuations::checker]
impl TypeChecker {
    /// Configure native synchronous-stack support for the compilation target.
    pub(crate) fn set_declaration_queries(
        &mut self,
        queries: std::rc::Rc<crate::compiler_db::declarations::DeclarationQueries>,
        unit: crate::module::UnitId,
    ) {
        self.declaration_queries = Some((queries, unit));
    }

    pub(crate) fn set_effect_queries(
        &mut self,
        queries: std::rc::Rc<crate::compiler_db::effects::EffectQueries>,
        unit: crate::module::UnitId,
    ) {
        self.capture_call_sites = queries.analysis.borrow().is_some();
        self.effect_queries = Some((queries, unit));
    }

    pub fn with_sync_stack_preemption(mut self, supported: bool) -> Self {
        self.task_sync_preemption = supported;
        self
    }

    pub fn new() -> Self {
        crate::query_stats::add(crate::query_stats::Counter::TypeChecker, 1);
        let resolution = std::rc::Rc::new(ResolutionContext {
            entry_program: false,
            module_path: None,
            class_module_path: None,
            task_sync_preemption: crate::compiler_db::inputs::TargetCapabilities::native()
                .sync_stack_preemption,
            imported_names: ObservedMap::new("resolution.imported_names"),
            module_access_names: ObservedMap::new("resolution.module_access_names"),
            registration_import_paths: std::rc::Rc::new(HashMap::new()),
            signature_only_modules: ObservedSet::new("resolution.signature_only_modules"),
            imported_collection_types: ObservedSet::new("resolution.imported_collection_types"),
            imported_collection_targets: ObservedSet::new("resolution.imported_collection_targets"),
            array_classes: ObservedMap::new("resolution.array_classes"),
            array_abi_files: ObservedSet::new("resolution.array_abi_files"),
            provider_template_files: ObservedSet::new("resolution.provider_template_files"),
            imported_collection_aliases: ObservedMap::new("resolution.imported_collection_aliases"),
            imported_std_modules: ObservedMap::new("resolution.imported_std_modules"),
            imported_blocking_std_functions: ObservedMap::new(
                "resolution.imported_blocking_std_functions",
            ),
            enforce_send_sync: false,
            field_writes: None,
            nonpreemptible_methods: Default::default(),
            nonpreemptible_module_methods: HashMap::new(),
            effect_index: Default::default(),
        });
        let mut checker = Self::empty(resolution, SymbolTable::default());
        checker.register_builtin_functions();
        checker.register_builtin_modules();
        checker.register_builtin_panic_surface();
        checker
    }

    fn empty(resolution: std::rc::Rc<ResolutionContext>, symbols: SymbolTable) -> Self {
        Self {
            defer_static_verification: false,
            generics: generics::Generics::default(),
            marker_contracts: ObservedMap::new("resolution.marker_contracts"),
            marker_contract_proven: ObservedSet::new("resolution.marker_contract_proven"),
            resolution,
            signature_metadata: false,
            body_queries: None,
            prechecks: None,
            declaration_cache: None,
            precheck_bodies: Default::default(),
            precheck_context: serde_json::Value::Null,
            precheck_occurrences: Default::default(),
            body_query_error: None,
            effect_queries: None,
            declaration_queries: None,
            checked_bodies: Vec::new(),
            reused_bodies: Vec::new(),
            lambdas: Default::default(),
            #[cfg(test)]
            inline_lambda_rechecks: 0,
            symbols,
            local: body::BodyState::default(),
            errors: Vec::new(),
            error_generation: 0,
            enum_variant_resolutions: HashMap::new(),
            pattern_resolutions: HashMap::new(),
            arm_leaves: HashMap::new(),
            expr_types: HashMap::new(),
            prechecked_array_args: HashMap::new(),
            reference_arg_modes: HashMap::new(),
            normalized_types: HashMap::new(),
            signature_reads: None,
            signature_qualified: HashSet::new(),
            signature_array_before: None,
            signature_normalized: HashMap::new(),
            signature_values: Default::default(),
            static_call_classes: HashMap::new(),
            lambda_captures: HashMap::new(),
            reported_class_cycles: ObservedSet::new("resolution.reported_class_cycles"),
            invalid_type_names: std::rc::Rc::new(std::cell::RefCell::new(ObservedSet::new(
                "resolution.invalid_type_names",
            ))),
            import_type_hints: Default::default(),
            fully_qualified_collection_types: ObservedSet::new(
                "resolution.fully_qualified_collection_types",
            ),
            missing_collection_imports_reported: ObservedSet::new(
                "resolution.missing_collection_imports_reported",
            ),
            resolved_calls: HashMap::new(),
            static_reads: HashMap::new(),
            analysis_calls: HashMap::new(),
            pending_constructor_flow: Vec::new(),
            analysis_symbols: Default::default(),
            member_uses: HashMap::new(),
            capture_call_sites: false,
            source_imports_checked: false,
            template_precheck: false,
            task_method_calls: Vec::new(),
            effect_inputs: Default::default(),
            indirect_lock: Default::default(),
            effect_graph_only: None,
        }
    }

    /// Name the module whose items this checker is about to check, so its own declarations get
    /// a canonical identity. Call
    /// before `check_module_program`; the entry program leaves it unset.
    pub(crate) fn capture_generic_call_targets(&mut self) {
        self.capture_call_sites = true;
    }

    pub fn set_module_path(&mut self, path: &str) {
        self.module_path = Some(path.to_string());
    }

    pub(crate) fn set_class_module_path(&mut self, path: &str) {
        self.class_module_path = Some(path.to_string());
    }

    /// Enable the Send/Sync async checks. Turned on when targeting multi-worker
    /// execution.
    pub fn set_enforce_send_sync(&mut self, on: bool) {
        self.enforce_send_sync = on;
    }

    /// Supply the whole-program field-write summary before registering any
    /// class, so imported and local `FieldInfo`s agree.
    pub(crate) fn set_field_writes(
        &mut self,
        writes: std::rc::Rc<crate::semantic::field_writes::FieldWrites>,
    ) {
        self.field_writes = Some(writes);
    }

    /// Whether an instance field may be written after construction.
    fn field_reassigned(&self, field: &FieldDecl) -> bool {
        let value = self
            .field_writes
            .as_ref()
            .is_some_and(|writes| writes.is_reassigned(&field.name, field.span));
        if precheck_journal::is_active() {
            precheck_journal::read(
                precheck_journal::Key::new(
                    "field_write",
                    Some(serde_json::json!([field.name, field.span])),
                ),
                value.into(),
            );
        }
        value
    }

    /// Seed the non-preemptible methods of imported classes (keyed by receiver
    /// class name -> source module display name and reason) so a cross-module
    /// typed-receiver call to one is flagged E0810. Call before
    /// `check_program`.
    pub fn set_nonpreemptible_module_methods(
        &mut self,
        methods: HashMap<FunctionId, (String, NonpreemptibleReason)>,
    ) {
        self.nonpreemptible_module_methods = methods;
    }

    /// The one name an enum answers to build-wide, given any spelling this unit
    /// can reach it by. Every other type name is its own identity
    /// and passes through: a class or interface is only ever registered by one
    /// unit's checker, while enums are registered by every unit that can see
    /// them and share one back-end table.
    pub(super) fn canonical_type_name(&self, name: &str) -> String {
        match self.symbols.lookup_enum(name) {
            Some(info) => info.name.clone(),
            None => name.to_string(),
        }
    }

    /// Whether this unit may write the enum's members with no enum name in
    /// front of them. A unit spells an enum bare when it declares it or when it
    /// item-imported it; one that only reaches the enum through
    /// `module::Enum` may not, so a module's variants stay invisible to its
    /// importers even though both now agree on the enum's identity.
    ///
    pub(super) fn enum_nameable_bare(&self, identity: &str) -> bool {
        self.symbols.enum_nameable_bare(identity)
    }

    /// Check a name as written in source, before normalization turns an alias
    /// into an identity. Do not apply this to inferred types or hierarchy
    /// lookups: those must be able to use transitive signature metadata.
    fn check_source_type_name(&mut self, name: &str, span: Span) -> bool {
        let Some((module, _)) = QualifiedName::new(&name).split_last() else {
            return true;
        };
        if self.signature_metadata
            || crate::module::std_registry::is_std_path(name)
            || self.symbols.lookup_class(name).is_some_and(|class| {
                self.array_abi_files
                    .contains(&class.declaration_span.file_id)
            })
            || !self.signature_only_modules.contains(module)
            || self.symbols.lookup_module(module).is_some()
        {
            return true;
        }
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0350,
                format!("cannot name type `{name}`: module `{module}` is not imported"),
            )
            .with_label(Label::primary(
                span,
                "type is outside this file's import scope",
            ))
            .with_help(format!(
                "import `{module}` in this file before naming its types"
            )),
        );
        false
    }

    fn check_source_type_access(&mut self, ty: &Type, span: Span) {
        match ty {
            Type::Named(name) => {
                self.check_source_type_name(name, span);
            }
            Type::Generic(name, args) => {
                self.check_source_type_name(name, span);
                for arg in args {
                    self.check_source_type_access(arg, span);
                }
            }
            Type::Array(element) | Type::FixedArray(element, _) => {
                self.check_source_type_access(element, span)
            }
            Type::Fn(params, ret) | Type::Closure(params, ret) => {
                for param in params {
                    self.check_source_type_access(param, span);
                }
                self.check_source_type_access(ret, span);
            }
            _ => {}
        }
    }

    fn normalize_type(&mut self, ty: &Type, span: Span) -> Type {
        self.record_signature_type(ty, span);
        if let Type::Generic(name, args) = ty
            && let Some(template) = self.generics.classes.get(name).cloned()
            && let Some(storage) = &template.abi_storage
        {
            if B::from_name(name) == Some(B::Array) {
                self.check_collection_type_imported(B::Array.source_name(), span);
            }
            if let Some((module, item)) = self.resolve_fully_qualified_std_item(name, span)
                && module == "collections"
            {
                self.record_signature_collection(item);
            }
            if args.len() != template.type_params.len() {
                self.generic_error(
                    span,
                    "wrong number of type arguments for bundled declaration",
                );
                return Self::error_type();
            }
            let normalized: Vec<_> = args
                .iter()
                .map(|arg| self.normalize_type(arg, span))
                .collect();
            let bindings = template
                .type_params
                .iter()
                .cloned()
                .zip(normalized)
                .collect();
            let concrete = crate::semantic::generic_inference::substitute(storage, &bindings);
            self.record_signature_normalized(ty, &concrete);
            return concrete;
        }
        if let Type::Generic(name, args) = ty
            && let Some(concrete) = self.instantiate_class(name, args, span)
        {
            self.record_signature_normalized(ty, &concrete);
            return concrete;
        }
        if let Type::Named(name) = ty
            && self.generics.classes.contains_key(name)
        {
            self.generic_error(
                span,
                format!("generic class `{name}` requires explicit type arguments"),
            );
            return Self::error_type();
        }

        // Declaration parameters shadow module types, including enum names
        // predeclared before registration. Preserve them for substitution.
        if let Type::Named(name) = ty
            && self.local.declared_type_params.contains(name)
        {
            return ty.clone();
        }
        if let Type::Named(name) | Type::Generic(name, _) = ty
            && !self.check_source_type_name(name, span)
        {
            return Type::Void;
        }
        let normalized = self.normalize_type_inner(ty, span);
        // A failed normalization answers `Void` after reporting (a malformed
        // builtin generic); recording that would hand lowering a type the
        // program never wrote. Everything else is the annotation's meaning.
        if normalized != *ty && !(normalized == Type::Void && *ty != Type::Void) {
            self.record_signature_normalized(ty, &normalized);
        }
        normalized
    }

    fn normalize_type_inner(&mut self, ty: &Type, span: Span) -> Type {
        // Instantiation aliases preserve the caller's scope until checked HIR
        // and backend signatures resolve the carried nominal declaration.
        if let Type::Named(name) = ty
            && name.starts_with("$type$mono$")
        {
            return ty.clone();
        }
        match ty {
            Type::FixedArray(element, n) => {
                Type::FixedArray(Box::new(self.normalize_type(element, span)), *n)
            }
            Type::Array(element) => {
                Type::Array(Box::new(self.normalize_type(element.as_ref(), span)))
            }
            Type::Generic(name, args) => {
                let args = args
                    .iter()
                    .map(|arg| self.normalize_type(arg, span))
                    .collect::<Vec<_>>();
                if B::from_name(name) == Some(B::Array) {
                    self.check_collection_type_imported(B::Array.source_name(), span);
                    return self.normalize_std_type_item(
                        name,
                        "collections",
                        B::Array.source_name(),
                        args,
                        span,
                    );
                }
                if let Some(item) = self.imported_collection_aliases.get(name).cloned() {
                    return self.normalize_std_type_item(name, "collections", &item, args, span);
                }
                if let Some((module, item)) = self.resolve_fully_qualified_std_item(name, span) {
                    if module == "collections" {
                        self.record_signature_collection(item.clone());
                    }
                    return self.normalize_std_type_item(name, &module, &item, args, span);
                }
                if let Some((module, item)) = self.resolve_imported_std_module_item(name, span) {
                    return self.normalize_std_type_item(name, &module, &item, args, span);
                }
                Type::Generic(
                    if name.starts_with("$type$mono$") {
                        name.clone()
                    } else {
                        self.canonical_type_name(name)
                    },
                    args,
                )
            }
            Type::Named(name) => {
                if self.imported_collection_aliases.contains_key(name) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("type `{name}` expects type arguments"),
                        )
                        .with_label(Label::primary(span, "missing type arguments")),
                    );
                    Type::Void
                } else if let Some((module, item)) =
                    self.resolve_fully_qualified_std_item(name, span)
                {
                    self.normalize_std_type_item(name, &module, &item, Vec::new(), span)
                } else if let Some((module, item)) =
                    self.resolve_imported_std_module_item(name, span)
                {
                    self.normalize_std_type_item(name, &module, &item, Vec::new(), span)
                } else {
                    match self.canonical_type_name(name) {
                        canonical if canonical == *name => ty.clone(),
                        canonical => Type::Named(canonical),
                    }
                }
            }
            Type::Fn(params, ret) => Type::Fn(
                params
                    .iter()
                    .map(|param| self.normalize_type(param, span))
                    .collect(),
                Box::new(self.normalize_type(ret.as_ref(), span)),
            ),
            Type::Closure(params, ret) => Type::Closure(
                params
                    .iter()
                    .map(|param| self.normalize_type(param, span))
                    .collect(),
                Box::new(self.normalize_type(ret.as_ref(), span)),
            ),
            Type::I64 | Type::F64 | Type::Bool | Type::String | Type::Void | Type::Never => {
                ty.clone()
            }
        }
    }

    fn normalize_param_types(&mut self, params: &[Param]) -> Vec<Type> {
        params
            .iter()
            .map(|param| self.normalize_type(&param.ty, param.type_span))
            .collect()
    }

    fn normalize_param_infos(&mut self, params: &[Param]) -> Vec<ParamInfo> {
        params
            .iter()
            .map(|param| ParamInfo {
                ty: self.normalize_type(&param.ty, param.type_span),
                mode: param.mode.clone(),
                span: param.span,
                type_span: param.type_span,
            })
            .collect()
    }

    fn normalize_std_type_item(
        &mut self,
        source_name: &str,
        module: &str,
        item: &str,
        args: Vec<Type>,
        span: Span,
    ) -> Type {
        let Some((expected, builtin_name)) = stdlib_schema::type_item(module, item) else {
            return Type::Generic(source_name.to_string(), args);
        };
        if args.len() != expected {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "`{source_name}` expects {expected} type argument{}, got {}",
                        if expected == 1 { "" } else { "s" },
                        args.len()
                    ),
                )
                .with_label(Label::primary(span, "wrong number of type arguments")),
            );
            // Do not manufacture a malformed builtin generic and let later
            // phases index missing arguments. The diagnostic above owns this
            // expression; Void is the compiler's established fail-safe type.
            return Type::Void;
        }

        if expected == 0 {
            Type::Named(builtin_name.to_string())
        } else if B::from_name(builtin_name) == Some(B::Array) {
            Type::Array(Box::new(
                args.into_iter().next().expect("validated Array arity"),
            ))
        } else {
            Type::Generic(builtin_name.to_string(), args)
        }
    }

    /// The source class owning one array storage type. The bundled module is
    /// registered before the modules its instantiations name, so those
    /// entries keep their canonical spelling instead of a declaration key.
    pub(super) fn array_class(&self, storage: &Type) -> Option<&String> {
        self.array_classes
            .get(&self.declaration_type_key(storage))
            .or_else(|| self.array_classes.get(storage))
    }

    /// Shared generic signatures use nominal aliases. Key storage dispatch by
    /// declaration identity so inferred local spellings and imported aliases agree.
    pub(super) fn declaration_type_key(&self, storage: &Type) -> Type {
        storage.map_names(|name| self.declaration_name_key(name))
    }

    /// One type name's declaration identity, the same in every unit and spelling.
    pub(super) fn declaration_name_key(&self, name: &str) -> String {
        let identity = self
            .symbols
            .lookup_class(name)
            .map(|info| (&info.name, info.declaration_span))
            .or_else(|| {
                self.symbols
                    .lookup_enum(name)
                    .map(|info| (&info.name, info.declaration_span))
            })
            .or_else(|| {
                self.symbols
                    .lookup_interface(name)
                    .map(|info| (&info.name, info.declaration_span))
            });
        identity.map_or_else(
            || name.to_owned(),
            |(identity, span)| {
                // Specializations share a declaration span but remain distinct.
                let specialization = identity.split_once("$mono$").map_or("", |(_, hash)| hash);
                format!(
                    "$declaration${}:{}:{specialization}",
                    span.file_id.0, span.start
                )
            },
        )
    }

    fn class_info_from_decl(
        &mut self,
        class: &ClassDecl,
        registered_name: &str,
        module_prefix: Option<&str>,
    ) -> ClassInfo {
        if let Some(storage) = &class.abi_storage {
            let storage = self.normalize_decl_type(storage, class.span, module_prefix);
            let key = self.declaration_type_key(&storage);
            self.array_classes.insert(key, registered_name.to_owned());
            self.array_abi_files.insert(class.span.file_id);
            self.normalized_types
                .insert(Type::Named(registered_name.to_owned()), storage);
        }
        let mut fields = HashMap::new();
        let mut methods = HashMap::new();
        let mut static_props = HashMap::new();
        let mut instance_field_order: Vec<(String, Type)> = Vec::new();

        for (decl_index, field) in class.fields.iter().enumerate() {
            let ty = self.normalize_decl_type(&field.ty, field.span, module_prefix);
            if field.is_static {
                // Static properties live in global storage, not instance layout.
                static_props.insert(
                    field.name.clone(),
                    StaticPropInfo {
                        ty,
                        is_mut: field.is_mut,
                        public: field.public,
                        protected: field.protected,
                        decl_index,
                        declaration_span: field.span,
                    },
                );
            } else {
                instance_field_order.push((field.name.clone(), ty.clone()));
                fields.insert(
                    field.name.clone(),
                    FieldInfo {
                        ty,
                        public: field.public,
                        protected: field.protected,
                        declaration_span: field.span,
                        reassigned: self.field_reassigned(field),
                    },
                );
            }
        }
        let constructor = class.constructors.first().map(|ctor| {
            let params = ctor
                .params
                .iter()
                .map(|p| self.normalize_decl_type(&p.ty, p.type_span, module_prefix))
                .collect();
            crate::semantic::symbols::ConstructorInfo {
                isolated_body: transfer_ownership::isolated_body(&ctor.body)
                    && ctor
                        .params
                        .iter()
                        .all(|p| !matches!(p.mode, ParamMode::Reference { .. })),
                param_infos: self.normalize_decl_param_infos(&ctor.params, module_prefix),
                params,
                public: ctor.public,
                protected: ctor.protected,
                declaration_span: ctor.span,
            }
        });
        for method in &class.methods {
            let outer_metadata = self.signature_metadata;
            self.signature_metadata |= method.is_interface_default;
            let params = method
                .params
                .iter()
                .map(|param| self.normalize_decl_type(&param.ty, param.type_span, module_prefix))
                .collect();
            methods.insert(
                method.name.clone(),
                MethodInfo {
                    isolated_body: transfer_ownership::isolated_body(&method.body),
                    effect_contracts: method.effect_contracts.clone(),
                    array_body_optimization: method.array_body_optimization,
                    param_infos: self.normalize_decl_param_infos(&method.params, module_prefix),
                    params,

                    is_static: method.is_static,
                    is_async: method.is_async,
                    return_type: self.normalize_decl_type(
                        &method.return_type,
                        method.span,
                        module_prefix,
                    ),
                    public: method.public,
                    protected: method.protected,
                    is_open: method.is_open,
                    is_override: method.is_override,
                    declaration_span: method.span,
                },
            );
            self.signature_metadata = outer_metadata;
        }

        ClassInfo {
            name: registered_name.to_string(),
            public: class.public,
            is_open: class.is_open,
            base_class: class
                .base_class
                .as_ref()
                .map(|base| qualified_type_path_name(base, module_prefix)),
            implements: class
                .implements
                .iter()
                .enumerate()
                .map(|(index, iface)| {
                    if index < class.source_implements_len {
                        self.normalize_decl_type(iface, class.span, module_prefix)
                    } else {
                        // Inherited contracts already name declarations in the
                        // dependency closure, not source imports of this file.
                        qualify_type_for_module(iface, module_prefix)
                            .map_names(|name| self.canonical_type_name(name))
                    }
                })
                .collect(),
            declaration_span: class.span,
            fields,
            methods,
            static_props,
            instance_field_order,
            constructor,
        }
    }

    fn normalize_decl_type(&mut self, ty: &Type, span: Span, module_prefix: Option<&str>) -> Type {
        if module_prefix.is_some() {
            qualify_type_for_module(ty, module_prefix)
        } else {
            self.normalize_type(ty, span)
        }
    }

    fn normalize_decl_param_infos(
        &mut self,
        params: &[Param],
        module_prefix: Option<&str>,
    ) -> Vec<ParamInfo> {
        params
            .iter()
            .map(|param| ParamInfo {
                ty: self.normalize_decl_type(&param.ty, param.type_span, module_prefix),
                mode: param.mode.clone(),
                span: param.span,
                type_span: param.type_span,
            })
            .collect()
    }

    fn base_class_requiring_initialization(&self, base_name: &str) -> Option<ClassInfo> {
        let mut current = Some(base_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return None;
            }
            let class = self.symbols.lookup_class(&name)?;
            if class.constructor.is_some() || !class.instance_field_order.is_empty() {
                return Some(class.clone());
            }
            current = class.base_class.clone();
        }
        None
    }

    /// No memberwise signature exists when an ancestor declares `init`: an
    /// intermediate subclass must supply its own initializer (E0848).
    fn implicit_constructor_field_infos(
        &self,
        class_name: &str,
    ) -> Option<Vec<(String, String, FieldInfo)>> {
        let mut chain = Vec::new();
        let mut current = Some(class_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                break;
            }
            let Some(class) = self.symbols.lookup_class(&name) else {
                break;
            };
            #[cfg(test)]
            subclass_constructor_tests::FIELD_CHAIN_VISITS.with(|n| n.set(n.get() + 1));
            if class.constructor.is_some() {
                return None;
            }
            let class_fields = class
                .instance_field_order
                .iter()
                .filter_map(|(field_name, _)| {
                    class
                        .fields
                        .get(field_name)
                        .cloned()
                        .map(|field| (field_name.clone(), field))
                })
                .collect::<Vec<_>>();
            // Keep the lookup owner, as lookup_field_in_hierarchy does: module
            // bodies bind their receiver under a local alias, while ClassInfo::name
            // can be qualified. Visibility compares against that receiver key.
            chain.push((name, class_fields));
            current = class.base_class.clone();
        }

        let mut fields = Vec::new();
        let mut names = HashSet::new();
        for (owner, class_fields) in chain.into_iter().rev() {
            for (name, field) in class_fields {
                if names.insert(name.clone()) {
                    fields.push((owner.clone(), name, field));
                }
            }
        }
        Some(fields)
    }

    /// Element type `T` of a `Channel<T>` used in a select case, or `Void` with a
    /// diagnostic if the operand is not a channel.
    fn select_channel_elem(&mut self, ch_ty: &Type, span: Span) -> Type {
        match channel_element_type(ch_ty) {
            Some(t) => t,
            None if Self::contains_error_type(ch_ty) => Self::error_type(),
            None => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0807,
                        format!(
                            "select case requires a `Channel<T>`, found `{}`",
                            self.diagnostic_type_name(ch_ty)
                        ),
                    )
                    .with_label(Label::primary(span, "not a channel")),
                );
                Type::Void
            }
        }
    }

    fn reference_place_info(&mut self, expr: &Expr, arg_span: Span) -> Option<ReferencePlaceInfo> {
        match expr {
            Expr::Var(name, _, _) => {
                self.record_binding_use(name, expr.span(), "reference-argument");
                let Some(var_info) = self.symbols.lookup_var(name).cloned() else {
                    self.check_expr(expr);
                    return None;
                };
                // Place checking bypasses check_expr, but the argument still
                // needs its authoritative type when lowered as a reference.
                self.expr_types.insert(expr.id(), var_info.ty.clone());
                Some(ReferencePlaceInfo {
                    name: name.clone(),
                    ty: var_info.ty,
                    mutable: var_info.mutable,
                    is_param: var_info.is_param,
                    declaration_span: var_info.declaration_span,
                    immutable_reason: None,
                })
            }
            Expr::FieldAccess(obj, field_name, span, _) => {
                let obj_ty = self.check_expr(obj);
                let field_ty = self.resolve_field(&obj_ty, field_name, *span, true, "read");
                // Reference arguments check places directly, bypassing
                // check_expr's type recording. Lowering still needs the
                // resolved field type, including imported specializations.
                self.expr_types.insert(expr.id(), field_ty.clone());
                if matches!(field_ty, Type::Void) {
                    return None;
                }
                // `PanicInfo` is runtime-owned panic metadata: direct assignment
                // is already rejected, and `&mut info.line` must not be a way
                // around that.
                let panic_info_field = obj_ty == Type::Named("PanicInfo".to_string());
                Some(ReferencePlaceInfo {
                    name: reference_place_key(expr).unwrap_or_else(|| field_name.clone()),
                    ty: field_ty,
                    mutable: !panic_info_field,
                    is_param: false,
                    declaration_span: *span,
                    immutable_reason: panic_info_field
                        .then_some("fields of `PanicInfo` are read-only"),
                })
            }
            Expr::Index(array, index, span, _) => {
                let elem_ty = self.check_index(array, index, *span);
                if matches!(elem_ty, Type::Void) {
                    return None;
                }
                self.expr_types.insert(expr.id(), elem_ty.clone());
                Some(ReferencePlaceInfo {
                    name: reference_place_key(expr).unwrap_or_else(|| "array element".to_string()),
                    ty: elem_ty,
                    mutable: true,
                    is_param: false,
                    declaration_span: *span,
                    immutable_reason: None,
                })
            }
            _ => {
                self.check_expr(expr);
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1704,
                    "cannot pass non-place expression by reference",
                )
                .with_label(Label::primary(arg_span, "not an assignable place"));

                if matches!(expr, Expr::Call(_)) {
                    diagnostic = diagnostic.with_help("function call results are temporaries");
                }

                self.push(diagnostic);
                None
            }
        }
    }

    fn push_mut_reference_alias_diagnostic(
        &mut self,
        name: &str,
        current_span: Span,
        previous_span: Span,
        previous_label: &'static str,
    ) {
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E1706,
                format!(
                    "cannot pass `{}` while it aliases a mutable reference",
                    name
                ),
            )
            .with_label(Label::primary(
                current_span,
                "same place aliases a mutable reference argument",
            ))
            .with_label(Label::secondary(previous_span, previous_label))
            .with_help("pass distinct mutable locals or split the call into separate steps"),
        );
    }

    fn lookup_field_in_hierarchy(
        &self,
        class_name: &str,
        field_name: &str,
    ) -> Option<(String, FieldInfo)> {
        let mut current = Some(class_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return None;
            }
            let class = self.symbols.lookup_class(&name)?;
            if let Some(field) = class.fields.get(field_name) {
                return Some((name, field.clone()));
            }
            current = class.base_class.clone();
        }
        None
    }

    fn lookup_method_in_hierarchy(
        &self,
        class_name: &str,
        method_name: &str,
    ) -> Option<(String, MethodInfo)> {
        let owner = self.resolved_method_class(class_name, method_name)?;
        let method = self
            .symbols
            .lookup_class(&owner)?
            .methods
            .get(method_name)?;
        Some((owner, method.clone()))
    }

    fn lookup_method_in_ancestors(
        &self,
        base_class_name: &str,
        method_name: &str,
    ) -> Option<(String, MethodInfo)> {
        self.lookup_method_in_hierarchy(base_class_name, method_name)
    }

    fn lookup_static_prop_in_hierarchy(
        &self,
        class_name: &str,
        prop_name: &str,
    ) -> Option<(String, StaticPropInfo)> {
        let mut current = Some(class_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return None;
            }
            let class = self.symbols.lookup_class(&name)?;
            if let Some(prop) = class.static_props.get(prop_name) {
                return Some((name, prop.clone()));
            }
            current = class.base_class.clone();
        }
        None
    }

    fn method_names_in_hierarchy(&self, class_name: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut current = Some(class_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                break;
            }
            let Some(class) = self.symbols.lookup_class(&name) else {
                break;
            };
            names.extend(class.methods.keys().cloned());
            current = class.base_class.clone();
        }
        names
    }

    fn unify_ternary_types(&self, then_ty: &Type, else_ty: &Type) -> Option<Type> {
        if Self::contains_error_type(then_ty) || Self::contains_error_type(else_ty) {
            return Some(Self::error_type());
        }
        if then_ty == else_ty {
            return Some(then_ty.clone());
        }

        if let Some(merged) = self.merge_inferred_enum_types(then_ty, else_ty) {
            return Some(merged);
        }
        match (then_ty, else_ty) {
            _ if self.types_compatible(then_ty, else_ty) => Some(then_ty.clone()),
            _ if self.types_compatible(else_ty, then_ty) => Some(else_ty.clone()),
            _ => None,
        }
    }

    /// Two values of distinct types that both convert to the type their
    /// expression flows into (return type, `let` annotation, argument) take
    /// that type, e.g. two classes implementing one interface.
    fn unify_to_expected(&self, expected: Option<&Type>, a: &Type, b: &Type) -> Option<Type> {
        let expected = expected?;
        #[cfg(test)]
        UNIFY_TO_EXPECTED_CALLS.with(|count| count.set(count.get() + 1));
        (self.types_compatible(expected, a) && self.types_compatible(expected, b))
            .then(|| expected.clone())
    }

    /// Help for a `match`/ternary whose values have incompatible types and no
    /// expected type: name a base class or interface of `a` that `b` also
    /// converts to, when one exists. Diagnostic path only.
    fn common_supertype_help(&self, a: &Type, b: &Type) -> String {
        let mut current = match a {
            Type::Named(name) => Some(name.clone()),
            _ => None,
        };
        let mut seen = HashSet::new();
        let mut common = None;
        while let Some(name) = current.take() {
            if !seen.insert(name.clone()) {
                break;
            }
            let Some(info) = self.symbols.lookup_class(&name) else {
                break;
            };
            let base = info.base_class.clone().map(Type::Named);
            common = info
                .implements
                .iter()
                .chain(base.iter())
                .find(|candidate| self.types_compatible(candidate, b))
                .cloned();
            if common.is_some() {
                break;
            }
            current = info.base_class.clone();
        }
        match common {
            Some(ty) => {
                let ty = self.diagnostic_type_name(&ty);
                format!(
                    "both values convert to `{ty}`; give the expression that type, e.g. `let value: {ty} = ...;` or a `-> {ty}` return type"
                )
            }
            None => "every value must have one type; annotate the destination (`let value: T = ...;`) with a type all values convert to".to_string(),
        }
    }

    fn merge_inferred_enum_types(&self, left: &Type, right: &Type) -> Option<Type> {
        if !self.generic_partially_matches(left, right) {
            return None;
        }
        let (Type::Generic(name, args), Type::Generic(_, other)) = (left, right) else {
            return None;
        };
        Some(Type::Generic(
            name.clone(),
            args.iter()
                .zip(other)
                .map(|(a, b)| {
                    if *a == Type::Void {
                        b.clone()
                    } else {
                        a.clone()
                    }
                })
                .collect(),
        ))
    }

    // Recovery-only nominal marker: cannot be spelled by source syntax and never
    // reaches lowering, since its creation requires an error diagnostic.
    fn error_type() -> Type {
        Type::Named("\0diagnostic-error".into())
    }
    fn is_error_type(ty: &Type) -> bool {
        matches!(ty, Type::Named(name) if Self::is_error_name(name))
    }
    /// Diagnostic consumers cannot use a partially recovered compound type either.
    /// Keep this separate from structural compatibility: a valid sibling type
    /// must still be checked when only one component is erroneous.
    fn contains_error_type(ty: &Type) -> bool {
        let mut current = ty;
        let mut pending: Vec<&[Type]> = Vec::new();
        loop {
            let ty = current;
            #[cfg(test)]
            RECOVERY_TYPE_VISITS.with(|visits| visits.set(visits.get() + 1));
            match ty {
                Type::Named(name) if Self::is_error_name(name) => return true,
                Type::Array(element) | Type::FixedArray(element, _) => {
                    current = element;
                    continue;
                }
                Type::Generic(name, args) => {
                    if Self::is_error_name(name) {
                        return true;
                    }
                    pending.push(args);
                }
                Type::Fn(params, result) | Type::Closure(params, result) => {
                    pending.push(std::slice::from_ref(result.as_ref()));
                    pending.push(params);
                }
                _ => {}
            }
            loop {
                let Some(siblings) = pending.last_mut() else {
                    return false;
                };
                if let Some((next, rest)) = siblings.split_first() {
                    current = next;
                    *siblings = rest;
                    break;
                }
                pending.pop();
            }
        }
    }

    fn is_error_name(name: &str) -> bool {
        name == "\0diagnostic-error"
    }

    fn unknown_type_help(&self, name: &str) -> String {
        let value = self.unknown_type_help_value(name);
        if precheck_journal::is_active() {
            precheck_journal::read(
                precheck_journal::Key::new("type_help", Some(name.into())),
                value.clone().into(),
            );
        }
        value
    }
    fn unknown_type_help_value(&self, name: &str) -> String {
        let hints = self.import_type_hints.get_or_init(|| {
            let mut hints: HashMap<String, Vec<String>> = HashMap::new();
            for (name, public) in self
                .symbols
                .classes
                .values()
                .map(|v| (&v.name, v.public))
                .chain(self.symbols.enums.values().map(|v| (&v.name, v.public)))
                .chain(
                    self.symbols
                        .interfaces
                        .values()
                        .map(|v| (&v.name, v.public)),
                )
            {
                if public && let Some((_, short)) = QualifiedName::new(&name).split_last() {
                    let names = hints.entry(short.to_owned()).or_default();
                    names.push(name.clone());
                    names.sort_unstable();
                    names.dedup();
                    names.truncate(5);
                }
            }
            hints
        });
        if let Some(names) = hints.get(name) {
            return format!(
                "import the type: {}",
                names
                    .iter()
                    .take(5)
                    .map(|n| format!("`import {n};`"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            );
        }
        // A fixed built-in vocabulary keeps typo recovery independent of the
        // number of declarations; exact missing-import hints above take priority.
        let builtins = [
            B::Array.source_name(),
            B::Map.source_name(),
            "FrozenArray",
            "Channel",
            "String",
            B::Option.source_name(),
            B::Result.source_name(),
            "Task",
        ];
        if let Some(suggestion) = suggest_similar_name(
            name,
            builtins.iter().filter(|candidate| **candidate != name),
        ) {
            return format!("did you mean `{suggestion}`?");
        }
        "define a class, enum, or interface with this name, or check the spelling".into()
    }

    fn validate_type(&mut self, ty: &Type, span: Span) {
        match ty {
            // Recovery and partial-inference markers are compiler-owned, not source names.
            ty if Self::is_error_type(ty)
                || crate::semantic::generic_inference::is_enum_hole(ty) => {}
            Type::FixedArray(element, _) => self.validate_type(element, span),
            Type::Array(element) => self.validate_type(element, span),
            Type::Generic(name, args) => {
                if builtin_types::is(ty, B::Map) {
                    self.check_collection_type_imported(B::Map.source_name(), span);
                }
                self.check_type_argument_count(name, args.len(), span);
                // A private generic is as private as a private plain type:
                // `m::Hidden<i64>` answers to the same rule as `m::Hidden`.
                self.check_type_visibility(name, span);
                for arg in args {
                    self.validate_type(arg, span);
                }
            }
            Type::Fn(params, ret) | Type::Closure(params, ret) => {
                for param in params {
                    self.validate_type(param, span);
                }
                self.validate_type(ret, span);
            }
            Type::I64 | Type::F64 | Type::Bool | Type::String | Type::Void | Type::Never => {}
            Type::Named(name)
                if matches!(
                    B::from_name(name),
                    Some(
                        B::AtomicI64
                            | B::AtomicBool
                            | B::CancellationToken
                            | B::TaskScope
                            | B::Cancelled
                    )
                ) || matches!(name.as_str(), "TcpListener" | "TcpStream") =>
            {
                // Compiler-known runtime primitives. TCP handles are opaque,
                // GC-managed objects constructed only by `std::net`.
            }
            // A type parameter of the declaration under validation: `T` in
            // `enum Wrap<T> { Val(T) }` is bound by the declaration itself.
            Type::Named(name) if self.local.declared_type_params.iter().any(|p| p == name) => {}
            Type::Named(name) => {
                // A named type must resolve to a known class or enum (including
                // module-qualified ones like `geometry::Point`, which are
                // registered under that key). Reject unknown names and module
                // names used as a type.
                if self.symbols.lookup_class(name).is_none()
                    && self.symbols.lookup_enum(name).is_none()
                    && self.symbols.lookup_interface(name).is_none()
                {
                    self.invalid_type_names.borrow_mut().insert(name.clone());
                    let diag = if self.symbols.lookup_module(name).is_some() {
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0350,
                            format!("`{name}` is a module, not a type"),
                        )
                        .with_label(Label::primary(span, "module used as a type"))
                        .with_help(format!(
                            "a module is a namespace; import a type from it or write `{name}::TypeName`"
                        ))
                    } else if let Some(arguments) = missing_builtin_type_arguments(name) {
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("type `{name}` requires type arguments"),
                        )
                        .with_label(Label::primary(span, "missing type arguments"))
                        .with_help(format!("write `{name}<{arguments}>` with concrete types"))
                    } else {
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0350,
                            format!("cannot find type `{name}`"),
                        )
                        .with_label(Label::primary(span, "not a known type"))
                        .with_help(self.unknown_type_help(name))
                    };
                    self.push(diag);
                }
                // A generic enum or interface is a type family, not a type: the
                // bare name supplies no type arguments, so nothing downstream can
                // instantiate it.
                self.check_type_argument_count(name, 0, span);
                self.check_type_visibility(name, span);
            }
        }
    }

    fn normalize_declared_type(&mut self, ty: &Type, type_params: &[String], span: Span) -> Type {
        let outer = std::mem::replace(&mut self.local.declared_type_params, type_params.to_vec());
        let normalized = self.normalize_type(ty, span);
        self.local.declared_type_params = outer;
        normalized
    }

    /// Validate `ty` as written inside a declaration that binds `type_params`. Runs after
    /// every declaration is registered, so a
    /// forward reference resolves, and with the parameters in scope, so `T`
    /// is a parameter rather than an unknown type.
    pub(super) fn validate_declared_type(&mut self, ty: &Type, type_params: &[String], span: Span) {
        let outer = std::mem::replace(&mut self.local.declared_type_params, type_params.to_vec());
        self.validate_type(ty, span);
        self.local.declared_type_params = outer;
    }

    /// Check a written type's type-argument count against the declaration
    /// its head names. Only declarations the symbol table owns
    /// are judged here; a compiler-known generic (`Map<K, V>`, `Task<T>`,
    /// `Mutex<T>`, ...) is not a symbol, and the std ones already have their
    /// arity checked when the spelling is normalized.
    ///
    /// Silently accepting a wrong count is what let `Option`, `Option<i64, i64>`
    /// and `Foo<i64>` reach codegen: the checker treated the first as an
    /// uninstantiated `Type::Named` (so `Some(..)` failed to resolve against it)
    /// and handed the others to the walker, which rejected them as an ICE.
    fn check_type_argument_count(&mut self, name: &str, given: usize, span: Span) {
        let (kind, params): (&str, Vec<String>) = if let Some(info) = self.symbols.lookup_enum(name)
        {
            ("enum", info.type_params.clone())
        } else if let Some(info) = self.symbols.lookup_interface(name) {
            ("interface", info.type_params.clone())
        } else if self.symbols.lookup_class(name).is_some() {
            ("class", Vec::new())
        } else if given > 0 && !compiler_known_generic_head(name) {
            // `Type::Named` reports its own unknown names; a generic head
            // that resolves to nothing is the same defect one spelling wider.
            self.invalid_type_names.borrow_mut().insert(name.to_owned());
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0350,
                    format!("cannot find type `{name}`"),
                )
                .with_label(Label::primary(span, "not a known type"))
                .with_help(self.unknown_type_help(name)),
            );
            return;
        } else {
            return;
        };
        let expected = params.len();
        if given == expected {
            return;
        }
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let verb = |n: usize| if n == 1 { "was" } else { "were" };
        let spelled = format!("{name}<{}>", params.join(", "));
        let diag = if expected == 0 {
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0201,
                format!(
                    "{kind} `{name}` is not generic, but {given} type argument{} {} given",
                    plural(given),
                    verb(given)
                ),
            )
            .with_label(Label::primary(span, "unexpected type arguments"))
            .with_help(format!("write `{name}` without type arguments"))
        } else if given == 0 {
            let example = std::iter::repeat_n("i64", expected)
                .collect::<Vec<_>>()
                .join(", ");
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0201,
                format!(
                    "{kind} `{name}` expects {expected} type argument{}, but none were given",
                    plural(expected)
                ),
            )
            .with_label(Label::primary(span, "missing type arguments"))
            .with_help(format!(
                "write `{spelled}` with concrete types, e.g. `{name}<{example}>`"
            ))
        } else {
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0201,
                format!(
                    "{kind} `{name}` expects {expected} type argument{}, but {given} {} given",
                    plural(expected),
                    verb(given)
                ),
            )
            .with_label(Label::primary(span, "wrong number of type arguments"))
            .with_help(format!("write `{spelled}`"))
        };
        self.push(diag);
    }

    /// Diagnose omitted Option/Result constructor parameters at an inference
    /// boundary. Contextual calls and method receivers may still resolve them.
    /// Also retains the binding diagnostic for fieldless user enum constructors.
    fn unresolved_generic_enum_diagnostic(
        &self,
        init: &Expr,
        ty: &Type,
        span: Span,
        var: &str,
    ) -> Option<Diagnostic> {
        // A user enum's fieldless constructor supplies no evidence for its
        // parameters. Diagnose the unresolved binding before it reaches LIR.
        if let Expr::StaticCall(call) = init
            && call.args.is_empty()
            && call.type_args.is_empty()
            && let Type::Generic(name, args) = ty
            && !matches!(B::from_name(name), Some(B::Option | B::Result))
            && args.contains(&Type::Void)
            && let Some(info) = self.symbols.lookup_enum(name)
            && info
                .variants
                .iter()
                .any(|variant| variant.name == call.method && variant.payload_types.is_empty())
        {
            return Some(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0202,
                    format!("cannot infer type arguments for `{name}::{}`", call.method),
                )
                .with_label(Label::primary(span, "type annotation required"))
                .with_help(format!("add a concrete type annotation to `{var}`")),
            );
        }
        // Only omitted parameters are holes. An explicitly written `void`, or
        // the inferred payload type of `Some(void_call())`, is a real type.
        let (name, variant) = match init {
            Expr::StaticCall(call) if call.type_args.is_empty() => {
                (call.class.as_str(), call.method.as_str())
            }
            Expr::Call(call)
                if matches!(call.callee.as_str(), "Ok" | "Err")
                    && self
                        .enum_variant_resolutions
                        .get(&call.id)
                        .is_some_and(|name| B::from_name(name) == Some(B::Result)) =>
            {
                (B::Result.source_name(), call.callee.as_str())
            }
            _ => return None,
        };
        let Type::Generic(actual_name, args) = ty else {
            return None;
        };
        if actual_name != name {
            return None;
        }
        #[cfg(test)]
        CONSTRUCTOR_INFERENCE_CHECKS.with(|checks| checks.set(checks.get() + 1));
        let var_label = if var == "_" { "x" } else { var };
        let (code, message, label, hint) = match (B::from_name(name), variant, args.as_slice()) {
            (Some(B::Option), "None", [Type::Void]) => (
                ErrorCode::E1801,
                "cannot infer type parameter `T` for `Option::None`".to_string(),
                "type annotation required",
                format!("add a type annotation, e.g. `let {var_label}: Option<i64> = ...;`"),
            ),
            (Some(B::Result), "Ok", [_, Type::Void]) => (
                ErrorCode::E1803,
                "cannot infer error type `E` for `Result::Ok`".to_string(),
                "error type is unknown",
                format!(
                    "add a type annotation, e.g. `let {var_label}: Result<i64, String> = ...;`"
                ),
            ),
            (Some(B::Result), "Err", [Type::Void, _]) => (
                ErrorCode::E1803,
                "cannot infer success type `T` for `Result::Err`".to_string(),
                "success type is unknown",
                format!(
                    "add a type annotation, e.g. `let {var_label}: Result<i64, String> = ...;`"
                ),
            ),
            _ => return None,
        };
        Some(
            Diagnostic::new(Severity::Error, code, message)
                .with_label(Label::primary(span, label))
                .with_help(hint),
        )
    }

    // Exact-type builtin contracts stay exact for valid values. On recovery,
    // compare known components so unrelated sibling mismatches remain visible.
    fn recovery_types_compatible(&self, expected: &Type, actual: &Type) -> bool {
        (Self::contains_error_type(expected) || Self::contains_error_type(actual))
            && self.same_type_identity(expected, actual)
    }

    fn types_compatible(&self, expected: &Type, actual: &Type) -> bool {
        // A diverging expression produces no value and can flow into any
        // expected type. This is directional: concrete values cannot become Never.
        Self::is_error_type(expected) || Self::is_error_type(actual)
            || *actual == Type::Never || self.same_type_identity(expected, actual)
            // A Void-placeholder generic (e.g. Option<Void> from None) matches any
            // concrete instantiation of the same generic enum.
            || matches!((expected, actual),
                (Type::Generic(en, _), Type::Generic(an, args))
                    if en == an && args.iter().all(|a| *a == Type::Void)
                        && self.symbols.lookup_enum(en).map(|e| !e.type_params.is_empty()).unwrap_or(false))
            // Result::Ok(v) produces Result<T,Void>; Result::Err(e) → Result<Void,E>
            // Accept if the non-Void type parameters match
            || self.generic_partially_matches(expected, actual)
            // An empty array literal `[]` produces `Array<Void>`, an unresolved
            // element type that a concrete `Array<T>` annotation resolves.
            || matches!((expected, actual),
                (Type::Array(e), Type::Array(a)) if **e == Type::Void || **a == Type::Void)
            // `Map::new()` produces `Map<Void, Void>`, resolved by the annotation.
            || (builtin_types::binary_args(expected, B::Map).is_some()
                && builtin_types::binary_args(actual, B::Map)
                    .is_some_and(|(key, value)| *key == Type::Void && *value == Type::Void))
            || self.is_subtype(actual, expected)
            || self.same_interface(expected, actual)
    }

    /// Structural equality through imported declaration aliases. Containers stay
    /// invariant: subtype conversion of an element does not convert its array.
    /// Visit each pair once, without allocating normalized copies of the types.
    fn same_type_identity(&self, mut expected: &Type, mut actual: &Type) -> bool {
        let name_eq = |a: &str, b: &str| {
            a == b
                || match (self.symbols.lookup_class(a), self.symbols.lookup_class(b)) {
                    // A shared instantiation alias may carry the declaring
                    // module's qualified spelling of a locally named instance.
                    // The instance name encodes its arguments, so one
                    // declaration with one unqualified name is one type.
                    (Some(a), Some(b)) => {
                        a.name == b.name
                            || (a.declaration_span == b.declaration_span
                                && QualifiedName::new(&a.name).last_segment()
                                    == QualifiedName::new(&b.name).last_segment())
                    }
                    _ => false,
                }
                || match (
                    self.symbols.lookup_interface(a),
                    self.symbols.lookup_interface(b),
                ) {
                    (Some(a), Some(b)) => a.name == b.name,
                    _ => false,
                }
                || match (self.symbols.lookup_enum(a), self.symbols.lookup_enum(b)) {
                    (Some(a), Some(b)) => a.name == b.name,
                    _ => false,
                }
        };
        let mut pending: Vec<(&[Type], &[Type])> = Vec::new();
        loop {
            let (a, b) = (expected, actual);
            #[cfg(test)]
            TYPE_IDENTITY_VISITS.with(|visits| visits.set(visits.get() + 1));
            let invalid = |ty: &Type| matches!(ty, Type::Named(n) | Type::Generic(n, _) if self.invalid_type_names.borrow().contains(n));
            match (a, b) {
                _ if invalid(a)
                    || invalid(b)
                    || Self::is_error_type(a)
                    || Self::is_error_type(b) => {}
                (Type::Named(a), Type::Named(b)) if name_eq(a, b) => {}
                (Type::FixedArray(a, n), Type::FixedArray(b, m)) if n == m => {
                    expected = a;
                    actual = b;
                    continue;
                }
                (Type::Array(a), Type::Array(b)) => {
                    expected = a;
                    actual = b;
                    continue;
                }
                (Type::Generic(a, aa), Type::Generic(b, ba))
                    if name_eq(a, b) && aa.len() == ba.len() =>
                {
                    pending.push((aa, ba));
                }
                (Type::Fn(aa, ar), Type::Fn(ba, br))
                | (Type::Closure(aa, ar), Type::Closure(ba, br))
                    if aa.len() == ba.len() =>
                {
                    pending.push((std::slice::from_ref(ar), std::slice::from_ref(br)));
                    pending.push((aa, ba));
                }
                (Type::I64, Type::I64)
                | (Type::F64, Type::F64)
                | (Type::Bool, Type::Bool)
                | (Type::String, Type::String)
                | (Type::Void, Type::Void)
                | (Type::Never, Type::Never) => {}
                // Unlike matching shapes, an unresolved subtree cannot support
                // a mismatch diagnostic. Keep checking any known siblings.
                _ if Self::contains_error_type(a) || Self::contains_error_type(b) => {}
                _ => return false,
            }
            // Retain sibling slices instead of one work item per sibling:
            // wide signatures use constant space, nested ones O(depth).
            loop {
                let Some((aa, ba)) = pending.pop() else {
                    return true;
                };
                let (Some((a, ar)), Some((b, br))) = (aa.split_first(), ba.split_first()) else {
                    continue;
                };
                if !ar.is_empty() {
                    pending.push((ar, br));
                }
                expected = a;
                actual = b;
                break;
            }
        }
    }

    /// Two spellings of ONE interface: `Describable`, bound bare by an item
    /// import, against the `proto::Describable` a module that imported it the
    /// same way now writes in its public signature. An interface
    /// is registered under the access spelling of whichever unit brought it in,
    /// so only its registered name is an identity both units share -- the
    /// reasoning `class_extends` already applies to classes.
    fn same_interface(&self, expected: &Type, actual: &Type) -> bool {
        let named = |ty: &Type| match ty {
            Type::Named(name) | Type::Generic(name, _) => self.symbols.lookup_interface(name),
            _ => None,
        };
        named(expected).is_some()
            && named(actual).is_some()
            && self.canonical_interface_type(expected) == self.canonical_interface_type(actual)
    }

    /// Allow `GenericEnum<Void, ...>` to match `GenericEnum<T, ...>` when
    /// Void is used as a placeholder for an unresolved type parameter.
    /// Only applied to generic enums registered in the symbol table (e.g. Option, Result).
    /// NOT applied to built-in non-enum generics like Channel, Future, Task, JoinHandle.
    fn generic_partially_matches(&self, expected: &Type, actual: &Type) -> bool {
        match (expected, actual) {
            (Type::Generic(en, eargs), Type::Generic(an, aargs)) if en == an => {
                // Only apply to registered generic enums (not Channel/Future/Task/JoinHandle)
                let is_enum = self
                    .symbols
                    .lookup_enum(en)
                    .map(|e| !e.type_params.is_empty())
                    .unwrap_or(false);
                is_enum
                    && eargs.len() == aargs.len()
                    && eargs
                        .iter()
                        .zip(aargs.iter())
                        .all(|(e, a)| e == a || *e == Type::Void || *a == Type::Void)
            }
            _ => false,
        }
    }

    fn is_subtype(&self, actual: &Type, expected: &Type) -> bool {
        // An interface default's opaque receiver converts to its interface,
        // and on from there.
        if let (Type::Named(name), Some(interface)) = (actual, &self.local.self_interface)
            && name == "Self"
        {
            return self.types_compatible(expected, interface);
        }
        match (actual, expected) {
            (Type::Named(child), Type::Named(parent)) => {
                // A class is a subtype of its base class, and of any interface it
                // implements (directly or through an ancestor); an interface is a
                // subtype of any interface it transitively extends.
                self.class_extends(child, parent)
                    || self.class_implements_interface(child, expected)
                    || self.interface_extends(child, parent)
            }
            // A class is a subtype of a generic interface instantiation it
            // implements, e.g. `Dog` <: `Box<String>`.
            (Type::Named(child), Type::Generic(_, _)) => {
                self.class_implements_interface(child, expected)
            }
            // A generic interface instantiation is a subtype of every
            // interface its declaration extends: `extends` names no type
            // arguments (E0427), so `Get<i64>` <: `Base`.
            (Type::Generic(child, _), Type::Named(parent)) => self.interface_extends(child, parent),
            _ => false,
        }
    }

    /// True when error type `e1` can be converted to `e2` for `?` automatic
    /// error conversion: `e1` is a concrete class implementing `Into<e2>`.
    ///
    fn err_converts_via_into(&self, e1: &Type, e2: &Type) -> bool {
        let Type::Named(e1_name) = e1 else {
            return false;
        };
        self.class_implements_interface(
            e1_name,
            &Type::Generic("Into".to_string(), vec![e2.clone()]),
        )
    }

    fn class_extends(&self, child: &str, parent: &str) -> bool {
        // Compare class identity by the registered (canonical) name so a
        // directly-imported subclass alias (`Dog`) is recognized as extending a
        // module base whose `base_class` is qualified (`shp::Animal`), and the
        // bare imported parent alias (`Animal`) matches it.
        let canon = |n: &str| -> String {
            self.symbols
                .lookup_class(n)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| n.to_string())
        };
        let parent_canon = canon(parent);
        let mut current = Some(child.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if canon(&name) == parent_canon {
                return true;
            }
            if !seen.insert(name.clone()) {
                return false;
            }
            let Some(class) = self.symbols.lookup_class(&name) else {
                return false;
            };
            let Some(base) = &class.base_class else {
                return false;
            };
            current = Some(base.clone());
        }
        false
    }

    /// The `extends` cycle on `class_name`'s base chain, as canonical names in
    /// chain order starting at the first class the walk met twice, or `None`
    /// when the chain ends at a class with no (registered) base. A class that
    /// merely extends INTO a cycle gets that cycle back too; the caller tells
    /// the two apart by membership.
    ///
    /// Identity is the registered `ClassInfo::name`, as in `class_extends`, so
    /// a chain that comes back through an imported alias still closes.
    fn class_extends_cycle(&self, class_name: &str) -> Option<Vec<String>> {
        let mut chain: Vec<String> = Vec::new();
        let mut current = Some(class_name.to_string());
        while let Some(name) = current {
            let class = self.symbols.lookup_class(&name)?;
            if let Some(start) = chain.iter().position(|seen| *seen == class.name) {
                return Some(chain.split_off(start));
            }
            chain.push(class.name.clone());
            current = class.base_class.clone();
        }
        None
    }

    fn type_mismatch_error_code(&self, expected: &Type, actual: &Type) -> ErrorCode {
        if self.is_class_type(expected) && self.is_class_type(actual) {
            ErrorCode::E0704
        } else {
            ErrorCode::E0201
        }
    }

    fn is_class_type(&self, ty: &Type) -> bool {
        match ty {
            Type::Named(name) => self.symbols.lookup_class(name).is_some(),
            _ => false,
        }
    }

    fn can_access_private_member(&self, owner: &str) -> bool {
        self.local.current_class.as_deref() == Some(owner)
    }

    /// Returns true when the current class is `owner` or a subclass of `owner`.
    fn can_access_protected_member(&self, owner: &str) -> bool {
        match self.local.current_class.as_deref() {
            Some(current) => current == owner || self.class_extends(current, owner),
            None => false,
        }
    }

    fn push(&mut self, d: Diagnostic) {
        if d.severity == Severity::Error {
            self.error_generation += 1;
        }
        self.errors.push(d);
    }

    /// An error with a single primary label, the most common diagnostic
    /// shape in the checker; callers may add helps, notes or fixes.
    fn error_at(
        code: ErrorCode,
        message: impl Into<String>,
        span: Span,
        label: impl Into<String>,
    ) -> Diagnostic {
        Diagnostic::new(Severity::Error, code, message).with_label(Label::primary(span, label))
    }

    /// Push [`Self::error_at`].
    fn push_error(
        &mut self,
        code: ErrorCode,
        message: impl Into<String>,
        span: Span,
        label: impl Into<String>,
    ) {
        self.push(Self::error_at(code, message, span, label));
    }

    /// Push "mismatched types: expected `E`, found `F`" labelled
    /// "expected `E`" at `span`, using the given error code.
    fn push_type_mismatch_with_code(
        &mut self,
        code: ErrorCode,
        expected: &Type,
        found: &Type,
        span: Span,
    ) {
        let expected = self.diagnostic_type_name(expected);
        let found = self.diagnostic_type_name(found);
        self.push_error(
            code,
            format!("mismatched types: expected `{expected}`, found `{found}`"),
            span,
            format!("expected `{expected}`"),
        );
    }

    /// [`Self::push_type_mismatch_with_code`] with the code chosen by
    /// [`Self::type_mismatch_error_code`].
    fn push_type_mismatch(&mut self, expected: &Type, found: &Type, span: Span) {
        let code = self.type_mismatch_error_code(expected, found);
        self.push_type_mismatch_with_code(code, expected, found, span);
    }

    /// Reject a non-`bool` `if`/`while` condition unless its expression
    /// already reported an error or recovered to the error type.
    fn check_condition_is_bool(&mut self, cond: &Expr, cond_ty: &Type, errors_before: usize) {
        if *cond_ty != Type::Bool
            && self.error_generation == errors_before
            && !Self::contains_error_type(cond_ty)
        {
            let found = self.diagnostic_type_name(cond_ty);
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0203,
                    format!("condition must be `bool`, found `{found}`"),
                )
                .with_label(Label::primary(
                    cond.span(),
                    format!("expected `bool`, found `{found}`"),
                ))
                .with_help("use an explicit comparison, e.g. `!= 0`"),
            );
        }
    }

    fn push_legacy_this_error(&mut self, span: Span) {
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0550,
                "receiver alias `this` is not supported",
            )
            .with_label(Label::primary(span, "`this` used as a receiver"))
            .with_help("use `self` inside instance methods"),
        );
    }
}

fn value_param_infos(params: &[Type]) -> Vec<ParamInfo> {
    params
        .iter()
        .map(|ty| ParamInfo {
            ty: ty.clone(),
            mode: ParamMode::Value,
            span: Span::dummy(),
            type_span: Span::dummy(),
        })
        .collect()
}

fn param_infos_from_decl(params: &[Param], module_prefix: Option<&str>) -> Vec<ParamInfo> {
    params
        .iter()
        .map(|param| ParamInfo {
            ty: qualify_type_for_module(&param.ty, module_prefix),
            mode: param.mode.clone(),
            span: param.span,
            type_span: param.type_span,
        })
        .collect()
}

/// Build the [`ClassInfo`] a MODULE exports for one of its classes.
///
/// `qualify` turns a type as the module wrote it into the name that type
/// answers to in the checker doing the registering: a module-local name gets
/// the module's spelling, and a name the module reached through an item import
/// of its own gets the key the DECLARING module's type is registered under. It replaces a
/// plain `module_prefix`, which prefixed every
/// bare name blindly and so exported `mid::Parcel` — a class no unit declares —
/// for a base, field, or parameter that came from `import base::Parcel;`.
fn imported_class_info_from_decl(
    class: &ClassDecl,
    registered_name: &str,
    qualify: &dyn Fn(&Type) -> Type,
    reassigned: &dyn Fn(&FieldDecl) -> bool,
) -> ClassInfo {
    let qualify_params = |params: &[Param]| -> Vec<ParamInfo> {
        params
            .iter()
            .map(|param| ParamInfo {
                ty: qualify(&param.ty),
                mode: param.mode.clone(),
                span: param.span,
                type_span: param.type_span,
            })
            .collect()
    };
    let mut fields = HashMap::new();
    let mut methods = HashMap::new();
    let mut static_props = HashMap::new();
    let mut instance_field_order: Vec<(String, Type)> = Vec::new();

    for (decl_index, field) in class.fields.iter().enumerate() {
        let ty = qualify(&field.ty);
        if field.is_static {
            static_props.insert(
                field.name.clone(),
                StaticPropInfo {
                    ty,
                    is_mut: field.is_mut,
                    public: field.public,
                    protected: field.protected,
                    decl_index,
                    declaration_span: field.span,
                },
            );
        } else {
            instance_field_order.push((field.name.clone(), ty.clone()));
            fields.insert(
                field.name.clone(),
                FieldInfo {
                    ty,
                    public: field.public,
                    protected: field.protected,
                    declaration_span: field.span,
                    reassigned: reassigned(field),
                },
            );
        }
    }
    let constructor = class.constructors.first().map(|ctor| {
        let params = ctor.params.iter().map(|p| qualify(&p.ty)).collect();
        crate::semantic::symbols::ConstructorInfo {
            isolated_body: transfer_ownership::isolated_body(&ctor.body)
                && ctor
                    .params
                    .iter()
                    .all(|p| !matches!(p.mode, ParamMode::Reference { .. })),
            param_infos: qualify_params(&ctor.params),
            params,
            public: ctor.public,
            protected: ctor.protected,
            declaration_span: ctor.span,
        }
    });
    for method in &class.methods {
        let params = method
            .params
            .iter()
            .map(|param| qualify(&param.ty))
            .collect();
        methods.insert(
            method.name.clone(),
            MethodInfo {
                isolated_body: transfer_ownership::isolated_body(&method.body),
                effect_contracts: method.effect_contracts.clone(),
                array_body_optimization: method.array_body_optimization,
                param_infos: qualify_params(&method.params),
                params,

                is_static: method.is_static,
                is_async: method.is_async,
                return_type: qualify(&method.return_type),
                public: method.public,
                protected: method.protected,
                is_open: method.is_open,
                is_override: method.is_override,
                declaration_span: method.span,
            },
        );
    }

    ClassInfo {
        name: registered_name.to_string(),
        public: class.public,
        is_open: class.is_open,
        base_class: class.base_class.as_ref().map(|base| {
            // Both spellings of a base go through `qualify`. A bare one for the
            // same reason a field type does: `extends Parcel` under
            // `import base::Parcel;` names the base module's class, not one of
            // this module's. A QUALIFIED one because the module wrote it in its
            // own spelling of that module -- `extends biz::Amount` -- which the
            // checker may have registered under another.
            let written = base.spelling();
            match &qualify(&Type::Named(written)) {
                Type::Named(qualified) => qualified.clone(),
                other => type_name(other),
            }
        }),
        implements: class.implements.iter().map(&qualify).collect(),
        declaration_span: class.span,
        fields,
        methods,
        static_props,
        instance_field_order,
        constructor,
    }
}

fn missing_builtin_type_arguments(name: &str) -> Option<&'static str> {
    match B::from_name(name) {
        Some(B::Task) => Some("()"),
        Some(B::Map | B::FrozenMap) => Some("K, V"),
        Some(B::Result) => Some("T, E"),
        Some(
            B::Option
            | B::TaskResult
            | B::Future
            | B::JoinHandle
            | B::Array
            | B::FrozenArray
            | B::Channel
            | B::BlockingCell
            | B::BlockingRwCell
            | B::Mutex
            | B::RwLock
            | B::Range,
        ) => Some("T"),
        _ => None,
    }
}

/// Whether a generic head is compiler-known without a symbol table entry.
fn compiler_known_generic_head(name: &str) -> bool {
    B::from_name(name).is_some()
}

#[cfg(test)]
#[path = "type_checker_tests.rs"]
mod tests;

#[cfg(test)]
mod continuation_depth_tests {
    use super::*;
    #[test]
    fn contextual_calls_and_ternaries_fifty_thousand_deep_on_one_mib() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let tokens = crate::lexer::Lexer::new(
                    "fn id(x: i64) -> i64 { return x; } fn f() -> i64 { return 7; }",
                )
                .tokenize()
                .unwrap();
                let (mut program, errors) = crate::parser::Parser::new(tokens).parse();
                assert!(errors.is_empty());
                let span = Span::dummy();
                let mut expression = Expr::Integer(7, span, ExprId::fresh());
                for index in 0..50_000 {
                    expression = if index % 2 == 0 {
                        Expr::Call(Box::new(CallExpr {
                            type_args: Vec::new(),
                            id: ExprId::fresh(),
                            callee: "id".into(),
                            args: vec![CallArg::value(expression)],
                            span,
                        }))
                    } else {
                        Expr::Ternary(Box::new(TernaryExpr {
                            id: ExprId::fresh(),
                            condition: Expr::Bool(true, span, ExprId::fresh()),
                            then_expr: expression,
                            else_expr: Expr::Integer(0, span, ExprId::fresh()),
                            span,
                        }))
                    };
                }
                let Item::Function(function) = &mut program.items[1] else {
                    unreachable!()
                };
                function.body.stmts = vec![Stmt::Return(ReturnStmt {
                    value: Some(expression),
                    span,
                })];
                let mut checker = TypeChecker::new();
                checker.check_program(&program);
                assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                assert!(checker.expr_types.len() >= 100_000);
                drop(checker);
                drop(program);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    fn nested_checker_sources(depth: usize) -> [String; 3] {
        let nested = |prefix: &str, leaf: &str, suffix: &str| {
            format!("{}{leaf}{}", prefix.repeat(depth), suffix.repeat(depth))
        };
        [
            format!(
                "fn gid<T>(x: T) -> T {{ return x; }} fn f() -> i64 {{ return {}; }}",
                nested("gid(", "7", ")")
            ),
            format!(
                "fn f() -> bool {{ return {}; }}",
                nested("match 1 { x if ", "true", " => true, _ => false }")
            ),
            format!(
                "fn apply<T>(x: T, f: fn(T) -> T) -> T {{ return f(x); }} fn f() -> i64 {{ return {}; }}",
                nested("apply(1, |a: i64| ", "a", ")")
            ),
        ]
    }

    /// Deepest executor nesting while checking `source`.
    fn checker_run_nesting(source: &str) -> usize {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut checker = TypeChecker::new();
        crate::compiler_stack::take_max_run_nesting();
        checker.check_program(&program);
        let nesting = crate::compiler_stack::take_max_run_nesting();
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        drop(checker);
        drop(program);
        nesting
    }

    /// generic calls, match guards and callable lambdas each
    /// re-entered a synchronous checker wrapper per nesting level.
    #[test]
    fn generic_calls_guards_and_callable_lambdas_deep_on_one_mib() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                for source in nested_checker_sources(2_000) {
                    checker_run_nesting(&source);
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// Executor nesting is independent of source nesting depth for every
    /// shape: each level is a heap child, not a fresh `compiler_stack::run`.
    #[test]
    fn nested_checker_shapes_keep_constant_executor_nesting() {
        for (shape, (one, many)) in nested_checker_sources(1)
            .iter()
            .zip(nested_checker_sources(64))
            .enumerate()
        {
            let (one, many) = (checker_run_nesting(one), checker_run_nesting(&many));
            println!("WILLOW_CHECKER_RUN_NESTING shape={shape} depth1={one} depth64={many}");
            assert_eq!(one, many, "shape {shape}");
        }
    }
}

mod analysis_symbols;

#[cfg(test)]
thread_local! {
    static TYPE_IDENTITY_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod type_identity_scaling_tests {
    use super::*;

    #[test]
    fn structural_identity_visits_each_node_once() {
        let mut checker = TypeChecker::new();
        let tokens = crate::lexer::Lexer::new("class Car {}").tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        checker.check_module_program(&program);
        let info = checker.symbols.lookup_class("Car").unwrap().clone();
        checker.symbols.define_class("Alias".into(), info);
        for size in [8, 32, 128, 512] {
            let mut chain = Type::Named("Car".into());
            let mut alias_chain = Type::Named("Alias".into());
            for _ in 0..size {
                chain = Type::Array(Box::new(chain));
                alias_chain = Type::Array(Box::new(alias_chain));
            }
            let wide = Type::Fn(vec![Type::Named("Car".into()); size], Box::new(Type::Bool));
            let alias_wide = Type::Fn(
                vec![Type::Named("Alias".into()); size],
                Box::new(Type::Bool),
            );
            for (ty, alias, expected) in [
                (&chain, &alias_chain, size + 1),
                (&wide, &alias_wide, size + 2),
            ] {
                TYPE_IDENTITY_VISITS.with(|v| v.set(0));
                for _ in 0..size {
                    assert!(checker.same_type_identity(ty, alias));
                }
                assert_eq!(TYPE_IDENTITY_VISITS.with(|v| v.get()), expected * size);
            }
        }
    }

    /// `implements` entries are matched against an alias-spelled generic target
    /// without allocating canonical copies: each entry costs one visit per node
    /// up to its first mismatch, so a lookup is linear in the entries' combined
    /// size.
    #[test]
    fn implements_lookup_visits_each_entry_node_once() {
        let mut checker = TypeChecker::new();
        let source = "interface P<T> { fn t(self, x: T) -> bool; } class Car {} class K {}";
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        checker.check_module_program(&program);
        let car = checker.symbols.lookup_class("Car").unwrap().clone();
        checker.symbols.define_class("Alias".into(), car);
        let nested = |depth: usize, leaf: &str| {
            let mut ty = Type::Named(leaf.into());
            for _ in 0..depth {
                ty = Type::Array(Box::new(ty));
            }
            Type::Generic("P".into(), vec![ty])
        };
        for (entries, depth) in [(4, 4), (16, 4), (64, 4), (16, 16), (16, 64)] {
            let mut k = checker.symbols.lookup_class("K").unwrap().clone();
            k.implements = (0..entries)
                .map(|i| nested(depth, &format!("Other{i}")))
                .chain(std::iter::once(nested(depth, "Car")))
                .collect();
            checker.symbols.define_class("K".into(), k);
            TYPE_IDENTITY_VISITS.with(|v| v.set(0));
            assert!(checker.class_implements_interface("K", &nested(depth, "Alias")));
            let visits = TYPE_IDENTITY_VISITS.with(|v| v.get());
            assert_eq!(visits, (entries + 1) * (depth + 2));
            eprintln!("implements entries={entries} depth={depth} visits={visits}");
        }
    }

    /// Interface conformance compares each required signature by identity in
    /// one structural pass: every parameter and return node is visited once
    /// per requirement, so checking a class is linear in its interface's
    /// signature size, however the class spells the types.
    #[test]
    fn conformance_visits_each_signature_node_once() {
        let parse = |source: &str| {
            let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
            let (program, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(errors.is_empty());
            program
        };
        for (methods, params) in [(4, 1), (16, 1), (64, 1), (16, 4), (16, 16)] {
            let args = |ty: &str| {
                (0..params)
                    .map(|p| format!(", a{p}: {ty}"))
                    .collect::<String>()
            };
            let iface = (0..methods)
                .map(|m| format!("fn m{m}(self{}) -> I;", args("I")))
                .collect::<String>();
            let class = (0..methods)
                .map(|m| {
                    format!(
                        "pub fn m{m}(self{}) -> Alias {{ return self; }}",
                        args("Alias")
                    )
                })
                .collect::<String>();
            // The class spells the interface by a second registered name, as
            // an importer spells a provider's interface by its access path.
            let mut checker = TypeChecker::new();
            checker.check_module_program(&parse(&format!("interface I {{ {iface} }}")));
            let info = checker.symbols.lookup_interface("I").unwrap().clone();
            checker
                .symbols
                .define_interface("Alias".into(), info.clone());
            let program = parse(&format!("class C implements I {{ {class} }}"));
            checker.check_module_program(&program);
            let Item::Class(class) = &program.items[0] else {
                panic!("expected class");
            };
            TYPE_IDENTITY_VISITS.with(|v| v.set(0));
            checker.check_interface_conformance(class, &info);
            assert!(checker.errors.is_empty(), "{:?}", checker.errors);
            let visits = TYPE_IDENTITY_VISITS.with(|v| v.get());
            assert_eq!(visits, methods * (params + 1));
            eprintln!("conformance methods={methods} params={params} visits={visits}");
        }
    }
}

#[cfg(test)]
mod contextual_arrays_tests;

#[cfg(test)]
thread_local! {
    static UNIFY_TO_EXPECTED_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod contextual_values_tests;

#[cfg(test)]
mod contextual_ctor_args_tests;

#[cfg(test)]
mod diagnostic_recovery_tests;

#[cfg(test)]
mod discarded_task_tests;

#[cfg(test)]
mod generic_lambda_tests;

#[cfg(test)]
mod loop_exit_arms_tests;

#[cfg(test)]
mod import_diagnostic_tests;

#[cfg(test)]
mod unsupported_syntax_diagnostic_tests;

#[cfg(test)]
thread_local! {
    static RECOVERY_TYPE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod generic_variant_tests;

#[cfg(test)]
mod constructor_inference_tests;

#[cfg(test)]
thread_local! {
    static CONSTRUCTOR_INFERENCE_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod fn_closure_diagnostic_tests;

/// A signature registered from another module keeps its source spelling, so a
/// bundled storage class (`Array<T>`) must become its storage type there, as
/// `normalize_type` does for a module's own annotations. `templates` holds only
/// the storage-backed class templates. A unit that never names `Array` itself
/// has no template for it, yet still meets it in an importer's signature;
/// that spelling is the bundled class, whose storage is the array type.
pub(super) fn bundled_storage_spelling(
    templates: &HashMap<String, std::rc::Rc<ClassDecl>>,
    ty: &Type,
) -> Type {
    ty.rewrite_generics(|name, mut args| {
        let template = templates.get(&name).or_else(|| {
            (name == "Array")
                .then(|| templates.get("$std::collections::Array"))
                .flatten()
        });
        match template {
            Some(template)
                if args.len() == template.type_params.len()
                    && let Some(storage) = &template.abi_storage =>
            {
                let bindings = template.type_params.iter().cloned().zip(args).collect();
                crate::semantic::generic_inference::substitute(storage, &bindings)
            }
            None if args.len() == 1
                && matches!(name.as_str(), "Array" | "$std::collections::Array") =>
            {
                Type::Array(Box::new(args.pop().expect("one argument")))
            }
            _ => Type::Generic(name, args),
        }
    })
}
