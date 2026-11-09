mod method_owners;
mod task_dispatch;

use crate::diagnostics::Span;
use crate::module::ModuleId;
use crate::parser::ast::{ParamMode, Type};
use crate::qualified_name::QualifiedName;
use crate::semantic::ids::{FunctionId, FunctionMap, TypeId};
use std::{collections::HashMap, rc::Rc};

/// Semantic declaration reads made while checking a body, including misses.
/// Spellings retain namespace distinctions and import aliases.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum SymbolRead {
    Function(String),
    Class(String),
    MethodOwner(String, String),
    TaskDispatch(String, String),
    Enum(String),
    EnumBare(String),
    Interface(String),
    Module(String),
    ModuleFunction(String, String),
    Dispatch(TypeId, String),
}

thread_local! {
    static BODY_READS: std::cell::RefCell<Vec<std::collections::HashSet<SymbolRead>>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub(crate) struct SymbolReadCapture;
impl SymbolReadCapture {
    pub(crate) fn begin() -> Self {
        BODY_READS.with(|reads| reads.borrow_mut().push(Default::default()));
        Self
    }
    pub(crate) fn current() -> Vec<SymbolRead> {
        BODY_READS.with(|reads| {
            reads
                .borrow()
                .last()
                .map(|reads| reads.iter().cloned().collect())
                .unwrap_or_default()
        })
    }
    pub(crate) fn finish(self) -> Vec<SymbolRead> {
        let reads = BODY_READS.with(|stack| {
            let mut stack = stack.borrow_mut();
            let reads = stack.pop().expect("body read capture");
            reads.into_iter().collect()
        });
        std::mem::forget(self);
        reads
    }
}
impl Drop for SymbolReadCapture {
    fn drop(&mut self) {
        BODY_READS.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}
/// Position-free key of one nominal declaration: its file and declared name
/// (without module qualification or a `$mono$` instance suffix). Reused
/// bodies keep the keys they were checked with, so the key must not move
/// when an edit shifts the declaration's offsets.
fn nominal_key_into(span: Span, declared: &str, key: &mut String) {
    use std::fmt::Write;
    let base = declared
        .split_once("$mono$")
        .map_or(declared, |(base, _)| base);
    let base = QualifiedName::new(&base)
        .split_last()
        .map_or(base, |(_, name)| name);
    let _ = write!(key, "@{}:{base}", span.file_id.0);
}

pub(crate) fn record_read(make: impl FnOnce() -> SymbolRead) {
    BODY_READS.with(|reads| {
        if let Some(reads) = reads.borrow_mut().last_mut() {
            reads.insert(make());
        }
    });
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnumVariantInfo<N = String> {
    pub name: String,
    pub payload_types: Vec<Type<N>>,
    pub tag: i64,
    pub declaration_span: Span,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnumInfo<N = String> {
    pub name: N,
    pub public: bool,
    /// Generic type parameter names in declaration order.
    /// Empty for non-generic enums.
    pub type_params: Vec<N>,
    pub variants: Vec<EnumVariantInfo<N>>,
    pub declaration_span: Span,
}

impl<N: Clone + Eq + std::hash::Hash> EnumInfo<N> {
    /// Instantiate a generic enum by substituting type arguments for parameters.
    /// Returns `self` unchanged if `type_params` is empty or `type_args` is empty.
    pub fn instantiate(&self, type_args: &[Type<N>]) -> Self {
        if self.type_params.is_empty() || type_args.is_empty() {
            return self.clone();
        }
        let param_map: std::collections::HashMap<N, Type<N>> = self
            .type_params
            .iter()
            .zip(type_args.iter())
            .map(|(p, a)| (p.clone(), a.clone()))
            .collect();
        EnumInfo {
            name: self.name.clone(),
            public: self.public,
            type_params: vec![],
            declaration_span: self.declaration_span,
            variants: self
                .variants
                .iter()
                .map(|v| EnumVariantInfo {
                    name: v.name.clone(),
                    tag: v.tag,
                    declaration_span: v.declaration_span,
                    payload_types: v
                        .payload_types
                        .iter()
                        .map(|t| substitute_type(t, &param_map))
                        .collect(),
                })
                .collect(),
        }
    }
}

pub fn substitute_type<N: Clone + Eq + std::hash::Hash>(
    ty: &Type<N>,
    param_map: &std::collections::HashMap<N, Type<N>>,
) -> Type<N> {
    ty.substitute_names(|name| param_map.get(name).cloned())
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VarInfo {
    pub ty: Type,
    pub mutable: bool,
    pub is_param: bool,
    pub declaration_span: Span,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FuncInfo {
    #[serde(default)]
    pub foreign: bool,
    pub params: Vec<Type>,
    pub param_infos: Vec<ParamInfo>,
    pub return_type: Type,
    pub public: bool,
    pub is_async: bool,
    pub declaration_span: Span,
    pub module_path: Option<String>,
    /// The value of a module-level `const`; `None` for every
    /// callable function. A constant's name is a value of `return_type`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constant: Option<crate::parser::ast::ConstValue>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ParamInfo<N = String> {
    pub ty: Type<N>,
    pub mode: ParamMode,
    pub span: Span,
    pub type_span: Span,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FieldInfo {
    pub ty: Type,
    pub public: bool,
    pub protected: bool,
    pub declaration_span: Span,
    /// Written somewhere after construction. Part of the
    /// declaration value, so a new write elsewhere invalidates reused bodies
    /// whose Send/Sync verdicts read this class.
    #[serde(default)]
    pub reassigned: bool,
}

/// An `init(...)` constructor's resolved signature. MVP allows at
/// most one constructor per class.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConstructorInfo {
    pub params: Vec<Type>,
    pub param_infos: Vec<ParamInfo>,
    pub public: bool,
    pub protected: bool,
    pub declaration_span: Span,
}

/// A `static [mut] name: T = expr` class property. Lives in global
/// storage, not instance layout.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StaticPropInfo {
    pub ty: Type,
    pub is_mut: bool,
    pub public: bool,
    pub protected: bool,
    /// Declaration index within the class, for init order / forward-reference
    /// checks (static member support §10.4). Populated now; read once §10.4 lands
    /// (tracked: static property initialization checks).
    #[allow(dead_code)]
    pub decl_index: usize,
    #[allow(dead_code)]
    pub declaration_span: Span,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MethodInfo {
    #[serde(default)]
    pub effect_contracts: Vec<crate::parser::ast::EffectContract>,
    #[serde(default)]
    pub array_body_optimization: crate::parser::ast::ArrayBodyOptimization,
    pub params: Vec<Type>,
    pub param_infos: Vec<ParamInfo>,
    /// `static fn` — class-level method with no receiver, called as
    /// `Type::method(...)`. Drives `::` vs `.` resolution.
    pub is_static: bool,
    pub is_async: bool,
    pub return_type: Type,
    pub public: bool,
    pub protected: bool,
    pub is_open: bool,
    #[allow(dead_code)]
    pub is_override: bool,
    pub declaration_span: Span,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClassInfo {
    pub name: String,
    pub public: bool,
    pub is_open: bool,
    pub base_class: Option<String>,
    /// Interfaces this class declares conformance to (`implements I, J`), as
    /// (module-qualified) types so generic interfaces carry their type args
    /// (e.g. `From<Err>`). Populated from `ClassDecl.implements`.
    pub implements: Vec<Type>,
    pub declaration_span: Span,
    pub fields: HashMap<String, FieldInfo>,
    pub methods: HashMap<String, MethodInfo>,
    /// `static [mut] name: T = expr` properties, keyed by name.
    pub static_props: HashMap<String, StaticPropInfo>,
    /// Instance fields in declaration order — drives the implicit memberwise
    /// constructor and definite-assignment checking.
    pub instance_field_order: Vec<(String, Type)>,
    /// The explicit `init(...)` constructor, if the class declares one. `None` means the
    /// implicit memberwise constructor applies.
    pub constructor: Option<ConstructorInfo>,
}

/// Nominal declarations carried with cross-module generic instantiations.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum InstantiationType {
    Class(ClassInfo),
    Enum(EnumInfo),
    Interface(InterfaceInfo),
}
impl InstantiationType {
    pub fn name(&self) -> &str {
        match self {
            Self::Class(i) => &i.name,
            Self::Enum(i) => &i.name,
            Self::Interface(i) => &i.name,
        }
    }

    pub fn declaration_span(&self) -> Span {
        match self {
            Self::Class(i) => i.declaration_span,
            Self::Enum(i) => i.declaration_span,
            Self::Interface(i) => i.declaration_span,
        }
    }
}

/// A required method signature declared inside an `interface`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InterfaceMethodInfo<N = String> {
    #[serde(default)]
    pub effect_contracts: Vec<crate::parser::ast::EffectContract>,
    pub name: String,
    pub params: Vec<Type<N>>,
    /// The same parameters with their declared [`ParamMode`], mirroring
    /// [`MethodInfo::param_infos`]. A `&`/`&mut` parameter is passed as a
    /// POINTER (`param_abi_type`), so the mode is part of the dispatch ABI:
    /// conformance, call checking and interface codegen all need it, and
    /// `params` alone cannot distinguish `value: i64` from `value: &mut i64`.
    ///
    pub param_infos: Vec<ParamInfo<N>>,
    pub is_static: bool,
    pub return_type: Type<N>,
    pub declaration_span: Span,
}

/// A registered `interface` declaration: a named set of required methods.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InterfaceInfo<N = String> {
    pub name: N,
    // `public`/`module_path` drive import visibility; `declaration_span`
    // feeds future diagnostics. Not read until those stages.
    #[allow(dead_code)]
    pub public: bool,
    pub methods: HashMap<String, InterfaceMethodInfo<N>>,
    /// Method names in declaration order — the deterministic vtable slot order
    /// used by interface dispatch codegen.
    pub method_order: super::method_slots::MethodSlots,
    /// Generic type parameter names in declaration order (`interface Foo<T>`),
    /// empty for non-generic interfaces.
    #[allow(dead_code)]
    pub type_params: Vec<N>,
    /// Direct super-interfaces (`interface B extends A`), module-qualified. Drives
    /// interface-to-interface subtyping; the inherited
    /// methods themselves are composed into `method_order` during desugaring.
    pub extends: Vec<N>,
    #[allow(dead_code)]
    pub declaration_span: Span,
    #[allow(dead_code)]
    pub module_path: Option<String>,
}

/// Functions declared by an imported module.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModuleInfo {
    pub functions: FunctionMap<FuncInfo>,
}

impl ModuleInfo {
    /// See [`FunctionMap::detached_clone`].
    pub fn detached_clone(&self) -> Self {
        Self {
            functions: self.functions.detached_clone(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeclarationSymbols {
    #[serde(with = "crate::compiler_db::map_entries")]
    pub functions: HashMap<FunctionId, FuncInfo>,
    #[serde(with = "crate::compiler_db::map_entries")]
    pub classes: HashMap<TypeId, ClassInfo>,
    pub modules: HashMap<ModuleId, ModuleInfo>,
    module_names: HashMap<String, ModuleId>,
    next_synthetic_module_id: u32,
    #[serde(with = "crate::compiler_db::map_entries")]
    pub enums: HashMap<TypeId, EnumInfo>,
    #[serde(with = "crate::compiler_db::map_entries")]
    pub interfaces: HashMap<TypeId, InterfaceInfo>,
}

impl Default for DeclarationSymbols {
    fn default() -> Self {
        Self {
            functions: HashMap::new(),
            classes: HashMap::new(),
            modules: HashMap::new(),
            module_names: HashMap::new(),
            next_synthetic_module_id: u32::MAX,
            enums: HashMap::new(),
            interfaces: HashMap::new(),
        }
    }
}

/// Body-local bindings share immutable declaration metadata. Legacy declaration
/// registration uses copy-on-write only when a declaration actually mutates.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SymbolTable {
    #[serde(skip)]
    precheck_values: Rc<std::cell::RefCell<HashMap<SymbolRead, serde_json::Value>>>,
    scopes: super::scopes::Scopes<VarInfo>,
    #[serde(skip)]
    method_owners: Rc<std::cell::RefCell<method_owners::MethodOwners>>,
    #[serde(skip)]
    task_dispatch: Rc<std::cell::RefCell<task_dispatch::TaskDispatch>>,
    #[serde(skip)]
    bare_enum_names: Rc<std::cell::RefCell<Option<std::collections::HashSet<String>>>>,
    #[serde(with = "declaration_rc")]
    declarations: Rc<DeclarationSymbols>,
}

impl std::ops::Deref for SymbolTable {
    type Target = DeclarationSymbols;
    fn deref(&self) -> &Self::Target {
        &self.declarations
    }
}

impl std::ops::DerefMut for SymbolTable {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.precheck_values = Default::default();
        self.bare_enum_names = Default::default();
        self.method_owners = Default::default();
        self.task_dispatch = Default::default();
        Rc::make_mut(&mut self.declarations)
    }
}

mod declaration_rc {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        value: &Rc<DeclarationSymbols>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(value.as_ref(), serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Rc<DeclarationSymbols>, D::Error> {
        serde::Deserialize::deserialize(deserializer).map(Rc::new)
    }
}

impl SymbolTable {
    /// The identity of a function type that is equal exactly when the checker
    /// considers the types the same (see [`Self::type_key`]); `None` for a
    /// non-function type or a function type without a full key.
    pub(crate) fn function_type_key(&self, ty: &Type) -> Option<String> {
        matches!(ty, Type::Fn(..) | Type::Closure(..))
            .then(|| self.type_key(ty))
            .flatten()
    }

    /// A key of `ty` that equal types share: nominal types are keyed by
    /// declaration without type arguments, so every spelling, import alias,
    /// `$type$mono$` alias and `Name$mono$` instance of one declaration
    /// agrees (an instance name carries no arguments to compare). Dropping
    /// arguments and sharing keys between `fn` and `closure` only merges
    /// keys. A part without a declaration (a type parameter, `Self`, a
    /// builtin container, an error or placeholder type) yields `None`, which
    /// callers treat as matching every key. Linear in the size of the type.
    pub(crate) fn type_key(&self, ty: &Type) -> Option<String> {
        let mut key = String::new();
        self.type_key_into(ty, &mut key)?;
        Some(key)
    }

    /// The key of a generic interface instantiation written `I<A, ..>`:
    /// the interface declaration plus each argument's [`Self::type_key`].
    /// Interfaces keep this spelling for receivers and `implements` clauses;
    /// any other spelling has no key.
    pub(crate) fn interface_instance_key(&self, ty: &Type) -> Option<String> {
        let Type::Generic(name, args) = ty else {
            return None;
        };
        let interface = self.lookup_interface(name)?;
        let mut key = String::new();
        nominal_key_into(interface.declaration_span, &interface.name, &mut key);
        key.push('<');
        for arg in args {
            self.type_key_into(arg, &mut key)?;
            key.push(',');
        }
        key.push('>');
        Some(key)
    }

    fn type_key_into(&self, ty: &Type, key: &mut String) -> Option<()> {
        match ty {
            Type::I64 => key.push('i'),
            Type::F64 => key.push('f'),
            Type::Bool => key.push('b'),
            Type::String => key.push('s'),
            Type::FixedArray(element, n) => {
                key.push_str(&format!("[{n};"));
                self.type_key_into(element, key)?;
                key.push(']');
            }
            Type::Array(element) => {
                key.push('[');
                self.type_key_into(element, key)?;
                key.push(']');
            }
            Type::Fn(params, ret) | Type::Closure(params, ret) => {
                key.push('(');
                for param in params {
                    self.type_key_into(param, key)?;
                    key.push(',');
                }
                key.push_str(")->");
                if **ret == Type::Void {
                    key.push('v');
                } else {
                    self.type_key_into(ret, key)?;
                }
            }
            Type::Named(name) | Type::Generic(name, _) => {
                let (span, declared) = self
                    .lookup_class(name)
                    .map(|info| (info.declaration_span, info.name.as_str()))
                    .or_else(|| {
                        self.lookup_enum(name)
                            .map(|info| (info.declaration_span, info.name.as_str()))
                    })
                    .or_else(|| {
                        self.lookup_interface(name)
                            .map(|info| (info.declaration_span, info.name.as_str()))
                    })?;
                nominal_key_into(span, declared, key);
            }
            Type::Void | Type::Never => return None,
        }
        Some(())
    }

    /// Current normalized value of one recorded lookup. Evaluated once per
    /// revision/key by the body reuse scheduler, never by scanning the scope.
    pub(crate) fn symbol_value(&self, read: &SymbolRead) -> serde_json::Value {
        let value = match read {
            SymbolRead::Dispatch(..) => unreachable!("dispatch is evaluated by BodyQueries"),
            SymbolRead::Function(name) => serde_json::to_value(self.lookup_func(name)),
            SymbolRead::Class(name) => serde_json::to_value(self.lookup_class(name)),
            SymbolRead::MethodOwner(class, method) => {
                serde_json::to_value(self.resolved_method_class(class, method))
            }
            SymbolRead::TaskDispatch(class, method) => {
                serde_json::to_value(self.dispatch_may_start_task(class, method))
            }
            SymbolRead::Enum(name) => serde_json::to_value(self.lookup_enum(name)),
            SymbolRead::EnumBare(name) => serde_json::to_value(self.enum_nameable_bare(name)),
            SymbolRead::Interface(name) => serde_json::to_value(self.lookup_interface(name)),
            SymbolRead::Module(name) => serde_json::to_value(self.lookup_module(name).is_some()),
            SymbolRead::ModuleFunction(module, name) => {
                serde_json::to_value(self.lookup_module_func(module, name))
            }
        }
        .expect("declaration serialization");
        crate::compiler_db::syntax::semantic(&value)
    }

    /// Start an independent body with no inherited local scopes. Sharing global
    /// declarations is O(1), regardless of the number of registered symbols.
    pub(crate) fn fork_body_scope(&self) -> Self {
        Self {
            scopes: Default::default(),
            precheck_values: Rc::clone(&self.precheck_values),
            bare_enum_names: Rc::clone(&self.bare_enum_names),
            method_owners: Rc::clone(&self.method_owners),
            task_dispatch: Rc::clone(&self.task_dispatch),
            declarations: Rc::clone(&self.declarations),
        }
    }

    /// Unit visibility without separately stored function/type payloads or
    /// the `shared` modules, which the caller restores from its own copy.
    pub(crate) fn declaration_shell(&self, shared: &[ModuleId]) -> Self {
        Self {
            scopes: Default::default(),
            precheck_values: Default::default(),
            bare_enum_names: Default::default(),
            method_owners: Default::default(),
            task_dispatch: Default::default(),
            declarations: Rc::new(DeclarationSymbols {
                modules: self
                    .modules
                    .iter()
                    .filter(|(id, _)| !shared.contains(id))
                    .map(|(id, info)| (*id, info.clone()))
                    .collect(),
                module_names: self.module_names.clone(),
                next_synthetic_module_id: self.next_synthetic_module_id,
                ..DeclarationSymbols::default()
            }),
        }
    }

    pub fn push_scope(&mut self) {
        self.scopes.push();
    }

    pub fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    pub fn define_var(&mut self, name: String, info: VarInfo) {
        self.scopes.insert(name, info);
    }

    /// Returns the existing binding in the innermost scope, if any.
    pub fn lookup_var_current_scope(&self, name: &str) -> Option<&VarInfo> {
        self.scopes.current(name)
    }

    pub fn lookup_var(&self, name: &str) -> Option<&VarInfo> {
        self.scopes.get(name)
    }

    pub fn lookup_var_mut(&mut self, name: &str) -> Option<&mut VarInfo> {
        self.scopes.get_mut(name)
    }

    pub fn define_func(&mut self, name: String, info: FuncInfo) {
        let read = SymbolRead::Function(name.clone());
        self.precheck_symbol_write(read.clone());
        let mut fingerprints = Rc::clone(&self.precheck_values);
        self.functions
            .insert(FunctionId::free_from_source_name(&name), info);
        Rc::make_mut(&mut fingerprints).get_mut().remove(&read);
        self.precheck_values = fingerprints;
    }

    pub fn lookup_func(&self, name: &str) -> Option<&FuncInfo> {
        let result = self.lookup_func_raw(name);
        self.precheck_symbol_read(SymbolRead::Function(name.to_owned()), result);
        result
    }
    fn lookup_func_raw(&self, name: &str) -> Option<&FuncInfo> {
        record_read(|| SymbolRead::Function(name.to_owned()));
        self.functions.get(&FunctionId::free_from_source_name(name))
    }

    pub fn define_class(&mut self, name: String, info: ClassInfo) {
        let read = SymbolRead::Class(name.clone());
        self.precheck_symbol_write(read.clone());
        let mut fingerprints = Rc::clone(&self.precheck_values);
        self.classes.insert(TypeId::from_source_name(&name), info);
        Rc::make_mut(&mut fingerprints).get_mut().remove(&read);
        self.precheck_values = fingerprints;
    }

    pub fn lookup_class(&self, name: &str) -> Option<&ClassInfo> {
        let result = self.lookup_class_raw(name);
        self.precheck_symbol_read(SymbolRead::Class(name.to_owned()), result);
        result
    }
    fn lookup_class_raw(&self, name: &str) -> Option<&ClassInfo> {
        record_read(|| SymbolRead::Class(name.to_owned()));
        self.classes.get(&TypeId::from_source_name(name))
    }

    pub fn define_module(&mut self, name: String, info: ModuleInfo) {
        let id = self.module_names.get(&name).copied().unwrap_or_else(|| {
            let id = ModuleId(self.next_synthetic_module_id);
            self.next_synthetic_module_id = self.next_synthetic_module_id.saturating_sub(1);
            id
        });
        self.define_module_with_id(name, id, info);
    }

    pub fn define_module_with_id(&mut self, name: String, id: ModuleId, info: ModuleInfo) {
        self.module_names.insert(name, id);
        self.modules.insert(id, info);
    }

    pub fn module_accesses(&self) -> impl Iterator<Item = (&str, &ModuleInfo)> {
        self.module_names
            .iter()
            .filter_map(|(name, id)| self.modules.get(id).map(|info| (name.as_str(), info)))
    }

    /// Remove a source spelling while retaining the shared module/type metadata.
    pub fn hide_module_spelling(&mut self, name: &str) {
        self.module_names.remove(name);
    }

    pub fn lookup_module(&self, name: &str) -> Option<&ModuleInfo> {
        let result = self.lookup_module_raw(name);
        self.precheck_symbol_read(SymbolRead::Module(name.to_owned()), result.is_some());
        result
    }
    fn lookup_module_raw(&self, name: &str) -> Option<&ModuleInfo> {
        record_read(|| SymbolRead::Module(name.to_owned()));
        self.modules.get(self.module_names.get(name)?)
    }

    pub(crate) fn lookup_module_func(&self, module: &str, name: &str) -> Option<&FuncInfo> {
        let result = self.lookup_module_func_raw(module, name);
        self.precheck_symbol_read(
            SymbolRead::ModuleFunction(module.to_owned(), name.to_owned()),
            result,
        );
        result
    }
    fn lookup_module_func_raw(&self, module: &str, name: &str) -> Option<&FuncInfo> {
        record_read(|| SymbolRead::ModuleFunction(module.to_owned(), name.to_owned()));
        self.modules
            .get(self.module_names.get(module)?)?
            .functions
            .get(name)
    }

    pub fn define_enum(&mut self, name: String, info: EnumInfo) {
        let read = SymbolRead::Enum(name.clone());
        self.precheck_symbol_write(read.clone());
        let mut fingerprints = Rc::clone(&self.precheck_values);
        self.enums.insert(TypeId::from_source_name(&name), info);
        Rc::make_mut(&mut fingerprints).get_mut().remove(&read);
        self.precheck_values = fingerprints;
    }

    pub fn lookup_enum(&self, name: &str) -> Option<&EnumInfo> {
        let result = self.lookup_enum_raw(name);
        self.precheck_symbol_read(SymbolRead::Enum(name.to_owned()), result);
        result
    }
    fn lookup_enum_raw(&self, name: &str) -> Option<&EnumInfo> {
        record_read(|| SymbolRead::Enum(name.to_owned()));
        self.enums.get(&TypeId::from_source_name(name))
    }

    pub(crate) fn enum_nameable_bare(&self, identity: &str) -> bool {
        let result = self.enum_nameable_bare_raw(identity);
        self.precheck_symbol_read(SymbolRead::EnumBare(identity.to_owned()), result);
        result
    }
    fn enum_nameable_bare_raw(&self, identity: &str) -> bool {
        record_read(|| SymbolRead::EnumBare(identity.to_owned()));
        self.bare_enum_names
            .borrow_mut()
            .get_or_insert_with(|| {
                self.enums
                    .iter()
                    // A `$type$mono$` instantiation alias is private, not a
                    // spelling the unit wrote.
                    .filter(|(alias, _)| {
                        let alias = alias.to_string();
                        !QualifiedName::new(&alias).is_qualified() && !alias.starts_with('$')
                    })
                    .map(|(_, info)| info.name.clone())
                    .collect()
            })
            .contains(identity)
    }

    pub fn define_interface(&mut self, name: String, info: InterfaceInfo) {
        let read = SymbolRead::Interface(name.clone());
        self.precheck_symbol_write(read.clone());
        let mut fingerprints = Rc::clone(&self.precheck_values);
        self.interfaces
            .insert(TypeId::from_source_name(&name), info);
        Rc::make_mut(&mut fingerprints).get_mut().remove(&read);
        self.precheck_values = fingerprints;
    }

    pub fn lookup_interface(&self, name: &str) -> Option<&InterfaceInfo> {
        let result = self.lookup_interface_raw(name);
        self.precheck_symbol_read(SymbolRead::Interface(name.to_owned()), result);
        result
    }
    fn lookup_interface_raw(&self, name: &str) -> Option<&InterfaceInfo> {
        record_read(|| SymbolRead::Interface(name.to_owned()));
        self.interfaces.get(&TypeId::from_source_name(name))
    }
}

impl EnumInfo {
    pub fn to_semantic(&self) -> EnumInfo<TypeId> {
        EnumInfo {
            name: TypeId::from_source_name(&self.name),
            public: self.public,
            type_params: self.type_params.iter().map(TypeId::local).collect(),
            declaration_span: self.declaration_span,
            variants: self
                .variants
                .iter()
                .map(|v| EnumVariantInfo {
                    name: v.name.clone(),
                    tag: v.tag,
                    declaration_span: v.declaration_span,
                    payload_types: v.payload_types.iter().map(Into::into).collect(),
                })
                .collect(),
        }
    }
}
impl InterfaceInfo {
    pub fn to_semantic(&self) -> InterfaceInfo<TypeId> {
        InterfaceInfo {
            name: TypeId::from_source_name(&self.name),
            public: self.public,
            type_params: self.type_params.iter().map(TypeId::local).collect(),
            extends: self
                .extends
                .iter()
                .map(|name| TypeId::from_source_name(name))
                .collect(),
            method_order: self.method_order.clone(),
            declaration_span: self.declaration_span,
            module_path: self.module_path.clone(),
            methods: self
                .methods
                .iter()
                .map(|(name, m)| {
                    (
                        name.clone(),
                        InterfaceMethodInfo {
                            effect_contracts: m.effect_contracts.clone(),
                            name: m.name.clone(),
                            params: m.params.iter().map(Into::into).collect(),
                            param_infos: m
                                .param_infos
                                .iter()
                                .map(|p| ParamInfo {
                                    ty: (&p.ty).into(),
                                    mode: p.mode.clone(),
                                    span: p.span,
                                    type_span: p.type_span,
                                })
                                .collect(),
                            is_static: m.is_static,
                            return_type: (&m.return_type).into(),
                            declaration_span: m.declaration_span,
                        },
                    )
                })
                .collect(),
        }
    }
}

impl SymbolTable {
    fn precheck_fingerprint(
        &self,
        read: &SymbolRead,
        value: impl FnOnce() -> serde_json::Value,
    ) -> serde_json::Value {
        use sha2::{Digest, Sha256};
        let cached = matches!(
            read,
            SymbolRead::Function(_)
                | SymbolRead::Class(_)
                | SymbolRead::Enum(_)
                | SymbolRead::Interface(_)
                | SymbolRead::ModuleFunction(..)
        );
        if cached && let Some(value) = self.precheck_values.borrow().get(read) {
            return value.clone();
        }
        let digest = Sha256::digest(
            serde_json::to_vec(&crate::compiler_db::syntax::semantic(&value()))
                .expect("symbol fingerprint"),
        );
        let value = serde_json::json!(format!("{:x}", crate::hex::Hex(&digest)));
        if cached {
            self.precheck_values
                .borrow_mut()
                .insert(read.clone(), value.clone());
        }
        value
    }
    pub(crate) fn precheck_value(&self, read: &SymbolRead) -> serde_json::Value {
        self.precheck_fingerprint(read, || self.symbol_value(read))
    }
    fn precheck_symbol_read(&self, read: SymbolRead, value: impl serde::Serialize) {
        use crate::semantic::type_checker::precheck_journal as journal;
        if journal::is_active() {
            let value = self.precheck_fingerprint(&read, || {
                serde_json::to_value(value).expect("symbol fingerprint")
            });
            journal::read(
                journal::Key::new("symbols", Some(serde_json::to_value(read).unwrap())),
                value,
            );
        }
    }
    fn precheck_dispatch_read(&self, read: SymbolRead, value: bool) {
        use crate::semantic::type_checker::precheck_journal as journal;
        if !journal::is_active() {
            return;
        }
        let writes = journal::take_dirty_symbols()
            .into_iter()
            .filter_map(|key| {
                let read: SymbolRead = serde_json::from_value(key.key.clone()?).ok()?;
                matches!(read, SymbolRead::Class(_) | SymbolRead::Interface(_)).then(|| {
                    let output = self.precheck_output(&read);
                    (key, output)
                })
            })
            .collect();
        journal::record_dispatch(journal::DispatchEvent {
            writes,
            read: serde_json::to_value(read).unwrap(),
            result: value,
        });
    }
    fn precheck_symbol_write(&self, read: SymbolRead) {
        use crate::semantic::type_checker::precheck_journal as journal;
        if journal::is_active() {
            let key = journal::Key::new("symbols", Some(serde_json::to_value(&read).unwrap()));
            let value = self.precheck_value(&read);
            journal::write(key, value);
        }
    }
    pub(crate) fn precheck_output(&self, read: &SymbolRead) -> serde_json::Value {
        match read {
            SymbolRead::Function(name) => serde_json::to_value(self.lookup_func(name)),
            SymbolRead::Class(name) => serde_json::to_value(self.lookup_class(name)),
            SymbolRead::Enum(name) => serde_json::to_value(self.lookup_enum(name)),
            SymbolRead::Interface(name) => serde_json::to_value(self.lookup_interface(name)),
            _ => return serde_json::Value::Null,
        }
        .unwrap()
    }
    pub(crate) fn restore_precheck(
        &mut self,
        read: SymbolRead,
        value: serde_json::Value,
    ) -> anyhow::Result<()> {
        match read {
            SymbolRead::Function(name) => self.define_func(name, serde_json::from_value(value)?),
            SymbolRead::Class(name) => self.define_class(name, serde_json::from_value(value)?),
            SymbolRead::Enum(name) => self.define_enum(name, serde_json::from_value(value)?),
            SymbolRead::Interface(name) => {
                self.define_interface(name, serde_json::from_value(value)?)
            }
            _ => anyhow::bail!("invalid precheck symbol output"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precheck_symbol_fingerprints_are_shared_and_invalidated_per_name() {
        for size in [8, 32, 128] {
            let mut symbols = SymbolTable::default();
            symbols.define_func("f".into(), function_info(Type::I64));
            let read = SymbolRead::Function("f".into());
            let encoded = std::cell::Cell::new(0);
            let fingerprint = |symbols: &SymbolTable| {
                symbols.precheck_fingerprint(&read, || {
                    encoded.set(encoded.get() + 1);
                    serde_json::json!(vec![0; size])
                })
            };
            let before = fingerprint(&symbols);
            for n in 0..size {
                symbols.define_func(format!("unrelated{n}"), function_info(Type::I64));
                assert_eq!(fingerprint(&symbols), before);
            }
            assert_eq!(encoded.get(), 1);
            let mut fork = symbols.fork_body_scope();
            fork.define_func("f".into(), function_info(Type::Bool));
            fingerprint(&fork);
            assert_eq!(encoded.get(), 2);
            assert_eq!(fingerprint(&symbols), before);
            assert_eq!(encoded.get(), 2);
            symbols.functions.clear();
            fingerprint(&symbols);
            assert_eq!(encoded.get(), 3);
        }
    }

    #[test]
    fn body_reads_include_missing_symbols_and_select_only_module_member() {
        let mut symbols = SymbolTable::default();
        symbols.define_module(
            "m".into(),
            ModuleInfo {
                functions: [
                    ("f", function_info(Type::I64)),
                    ("g", function_info(Type::Bool)),
                ]
                .into_iter()
                .collect(),
            },
        );
        let capture = SymbolReadCapture::begin();
        assert!(symbols.lookup_func("missing").is_none());
        assert!(symbols.lookup_module("m").is_some());
        assert!(symbols.lookup_module_func("m", "f").is_some());
        assert!(symbols.lookup_module_func("m", "f").is_some());
        let reads: std::collections::HashSet<_> = capture.finish().into_iter().collect();
        assert_eq!(reads.len(), 3);
        assert!(reads.contains(&SymbolRead::Function("missing".into())));
        assert!(reads.contains(&SymbolRead::ModuleFunction("m".into(), "f".into())));
        assert!(!reads.contains(&SymbolRead::ModuleFunction("m".into(), "g".into())));
        let missing = symbols.symbol_value(&SymbolRead::Function("missing".into()));
        symbols.define_func("missing".into(), function_info(Type::I64));
        assert_ne!(
            missing,
            symbols.symbol_value(&SymbolRead::Function("missing".into()))
        );
    }

    #[test]
    fn module_aliases_share_one_stable_module_identity() {
        let mut symbols = SymbolTable::default();
        let id = ModuleId(12);
        symbols.define_module_with_id("network".into(), id, ModuleInfo::default());
        symbols.define_module_with_id("net".into(), id, ModuleInfo::default());

        assert!(symbols.lookup_module("network").is_some());
        assert!(symbols.lookup_module("net").is_some());
        assert_eq!(symbols.modules.len(), 1);
    }

    #[test]
    fn function_and_type_tables_store_typed_keys() {
        let symbols = SymbolTable::default();
        let _: &HashMap<FunctionId, FuncInfo> = &symbols.functions;
        let _: &HashMap<TypeId, ClassInfo> = &symbols.classes;
    }
    fn function_info(ty: Type) -> FuncInfo {
        FuncInfo {
            foreign: false,
            params: vec![],
            param_infos: vec![],
            return_type: ty,
            public: true,
            is_async: false,
            declaration_span: Span::dummy(),
            module_path: None,
            constant: None,
        }
    }

    fn variable_info(ty: Type) -> VarInfo {
        VarInfo {
            ty,
            mutable: true,
            is_param: false,
            declaration_span: Span::dummy(),
        }
    }

    #[test]
    fn body_forks_share_declarations_and_isolate_local_bindings() {
        for size in [16, 64, 256, 1024] {
            let mut declarations = SymbolTable::default();
            for index in 0..size {
                declarations.define_func(format!("f{index}"), function_info(Type::I64));
            }
            declarations.push_scope();
            declarations.define_var("outer".into(), variable_info(Type::Bool));
            let mut forks = Vec::with_capacity(size);
            for index in 0..size {
                let mut body = declarations.fork_body_scope();
                assert!(body.lookup_var("outer").is_none());
                body.push_scope();
                body.define_var(format!("local{index}"), variable_info(Type::I64));
                body.lookup_var_mut(&format!("local{index}")).unwrap().ty = Type::Bool;
                assert!(Rc::ptr_eq(&declarations.declarations, &body.declarations));
                assert_eq!(body.functions.len(), size);
                forks.push(body);
            }
            assert_eq!(Rc::strong_count(&declarations.declarations), size + 1);
            assert!(forks[0].lookup_var("local1").is_none());
            assert!(declarations.lookup_var("local0").is_none());
            forks[0].pop_scope();
            assert!(forks[0].lookup_var("local0").is_none());
            eprintln!("body-symbols declarations={size} forks={size} shared_allocations=1");
        }
    }

    #[test]
    fn declaration_mutation_copies_only_the_mutating_fork() {
        let mut original = SymbolTable::default();
        original.define_func("f".into(), function_info(Type::I64));
        original.define_module_with_id("module".into(), ModuleId(1), ModuleInfo::default());
        let mut fork = original.fork_body_scope();
        let sibling = original.fork_body_scope();
        fork.define_func("f".into(), function_info(Type::Bool));
        fork.hide_module_spelling("module");
        assert!(!Rc::ptr_eq(&original.declarations, &fork.declarations));
        assert!(Rc::ptr_eq(&original.declarations, &sibling.declarations));
        assert_eq!(original.lookup_func("f").unwrap().return_type, Type::I64);
        assert_eq!(fork.lookup_func("f").unwrap().return_type, Type::Bool);
        assert!(original.lookup_module("module").is_some());
        assert!(fork.lookup_module("module").is_none());
    }

    #[test]
    fn shared_declarations_roundtrip_canonical_functions_and_module_aliases() {
        let mut symbols = SymbolTable::default();
        symbols.define_func("pkg::run".into(), function_info(Type::I64));
        let mut module = ModuleInfo::default();
        module
            .functions
            .insert("pkg::run", function_info(Type::Bool));
        symbols.define_module_with_id("pkg".into(), ModuleId(9), module.clone());
        symbols.define_module_with_id("alias".into(), ModuleId(9), module);
        symbols.push_scope();
        symbols.define_var("local".into(), variable_info(Type::I64));
        let wire = serde_json::to_vec(&symbols).unwrap();
        let restored: SymbolTable = serde_json::from_slice(&wire).unwrap();
        assert_eq!(
            restored.lookup_func("pkg::run").unwrap().return_type,
            Type::I64
        );
        assert!(std::ptr::eq(
            restored.lookup_module("pkg").unwrap(),
            restored.lookup_module("alias").unwrap()
        ));
        assert_eq!(
            restored
                .lookup_module("alias")
                .unwrap()
                .functions
                .get("pkg::run")
                .unwrap()
                .return_type,
            Type::Bool
        );
        assert_eq!(restored.lookup_var("local").unwrap().ty, Type::I64);
        assert_eq!(restored.next_synthetic_module_id, u32::MAX);
    }
}
