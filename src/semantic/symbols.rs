use crate::diagnostics::Span;
use crate::module::ModuleId;
use crate::parser::ast::{ParamMode, Type};
use crate::semantic::ids::{FunctionId, FunctionMap, TypeId};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct EnumVariantInfo {
    pub name: String,
    pub payload_types: Vec<Type>,
    pub tag: i64,
    pub declaration_span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumInfo {
    pub name: String,
    pub public: bool,
    /// Generic type parameter names in declaration order.
    /// Empty for non-generic enums.
    pub type_params: Vec<String>,
    pub variants: Vec<EnumVariantInfo>,
    pub declaration_span: Span,
}

impl EnumInfo {
    /// Instantiate a generic enum by substituting type arguments for parameters.
    /// Returns `self` unchanged if `type_params` is empty or `type_args` is empty.
    pub fn instantiate(&self, type_args: &[Type]) -> Self {
        if self.type_params.is_empty() || type_args.is_empty() {
            return self.clone();
        }
        let param_map: std::collections::HashMap<String, Type> = self
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

pub fn substitute_type(ty: &Type, param_map: &std::collections::HashMap<String, Type>) -> Type {
    match ty {
        Type::Named(n) => {
            if let Some(replacement) = param_map.get(n) {
                replacement.clone()
            } else {
                ty.clone()
            }
        }
        Type::Generic(name, args) => Type::Generic(
            name.clone(),
            args.iter().map(|a| substitute_type(a, param_map)).collect(),
        ),
        Type::Nullable(inner) => Type::Nullable(Box::new(substitute_type(inner, param_map))),
        Type::Array(inner) => Type::Array(Box::new(substitute_type(inner, param_map))),
        Type::Fn(params, ret) => Type::Fn(
            params
                .iter()
                .map(|p| substitute_type(p, param_map))
                .collect(),
            Box::new(substitute_type(ret, param_map)),
        ),
        _ => ty.clone(),
    }
}

#[derive(Debug, Clone)]
pub struct VarInfo {
    pub ty: Type,
    pub mutable: bool,
    pub is_param: bool,
    pub declaration_span: Span,
}

#[derive(Debug, Clone)]
pub struct FuncInfo {
    pub params: Vec<Type>,
    pub param_infos: Vec<ParamInfo>,
    pub return_type: Type,
    pub public: bool,
    pub is_async: bool,
    pub declaration_span: Span,
    pub module_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ParamInfo {
    pub ty: Type,
    pub mode: ParamMode,
    pub span: Span,
    pub type_span: Span,
}

#[derive(Debug, Clone)]
pub struct FieldInfo {
    pub ty: Type,
    pub public: bool,
    pub protected: bool,
    pub declaration_span: Span,
}

/// An `init(...)` constructor's resolved signature (willow-scq2). MVP allows at
/// most one constructor per class.
#[derive(Debug, Clone)]
pub struct ConstructorInfo {
    pub params: Vec<Type>,
    pub param_infos: Vec<ParamInfo>,
    pub public: bool,
    pub protected: bool,
    pub declaration_span: Span,
}

/// A `static [mut] name: T = expr` class property (willow-qsqf). Lives in global
/// storage, not instance layout.
#[derive(Debug, Clone)]
pub struct StaticPropInfo {
    pub ty: Type,
    pub is_mut: bool,
    pub public: bool,
    pub protected: bool,
    /// Declaration index within the class, for init order / forward-reference
    /// checks (willow-qsqf §10.4). Populated now; read once §10.4 lands
    /// (tracked: willow-pz6q.9).
    #[allow(dead_code)]
    pub decl_index: usize,
    #[allow(dead_code)]
    pub declaration_span: Span,
}

#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub params: Vec<Type>,
    pub param_infos: Vec<ParamInfo>,
    /// Records an explicit (legacy) `self` param; `is_static` drives resolution.
    #[allow(dead_code)]
    pub has_self: bool,
    /// `static fn` — class-level method with no receiver, called as
    /// `Type::method(...)` (willow-qsqf). Drives `::` vs `.` resolution instead
    /// of `has_self`, which now only records an explicit (legacy) `self` param.
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

#[derive(Debug, Clone)]
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
    /// `static [mut] name: T = expr` properties (willow-qsqf), keyed by name.
    pub static_props: HashMap<String, StaticPropInfo>,
    /// Instance fields in declaration order — drives the implicit memberwise
    /// constructor and definite-assignment checking (willow-scq2).
    pub instance_field_order: Vec<(String, Type)>,
    /// The explicit `init(...)` constructor, if the class declares one
    /// (willow-scq2). `None` means the implicit memberwise constructor applies.
    pub constructor: Option<ConstructorInfo>,
}

/// A required method signature declared inside an `interface`.
#[derive(Debug, Clone)]
pub struct InterfaceMethodInfo {
    pub name: String,
    pub params: Vec<Type>,
    pub has_self: bool,
    pub return_type: Type,
    pub declaration_span: Span,
}

/// A registered `interface` declaration: a named set of required methods.
#[derive(Debug, Clone)]
pub struct InterfaceInfo {
    pub name: String,
    // `public`/`module_path` drive import visibility (willow-k6g); `declaration_span`
    // feeds future diagnostics. Not read until those stages.
    #[allow(dead_code)]
    pub public: bool,
    pub methods: HashMap<String, InterfaceMethodInfo>,
    /// Method names in declaration order — the deterministic vtable slot order
    /// used by interface dispatch codegen (willow-xds).
    pub method_order: Vec<String>,
    /// Generic type parameter names in declaration order (`interface Foo<T>`),
    /// empty for non-generic interfaces (willow-1js.1).
    #[allow(dead_code)]
    pub type_params: Vec<String>,
    /// Direct super-interfaces (`interface B extends A`), module-qualified
    /// (willow-1js.2). Drives interface-to-interface subtyping; the inherited
    /// methods themselves are composed into `method_order` during desugaring.
    pub extends: Vec<String>,
    #[allow(dead_code)]
    pub declaration_span: Span,
    #[allow(dead_code)]
    pub module_path: Option<String>,
}

/// Functions declared by an imported module.
#[derive(Debug, Default, Clone)]
pub struct ModuleInfo {
    pub functions: FunctionMap<FuncInfo>,
}

#[derive(Debug)]
pub struct SymbolTable {
    scopes: Vec<HashMap<String, VarInfo>>,
    pub functions: HashMap<FunctionId, FuncInfo>,
    pub classes: HashMap<TypeId, ClassInfo>,
    pub modules: HashMap<ModuleId, ModuleInfo>,
    module_names: HashMap<String, ModuleId>,
    next_synthetic_module_id: u32,
    pub enums: HashMap<TypeId, EnumInfo>,
    pub interfaces: HashMap<TypeId, InterfaceInfo>,
}

impl Default for SymbolTable {
    fn default() -> Self {
        Self {
            scopes: Vec::new(),
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

impl SymbolTable {
    pub fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    pub fn define_var(&mut self, name: String, info: VarInfo) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name, info);
        }
    }

    /// Returns the existing binding in the innermost scope, if any.
    pub fn lookup_var_current_scope(&self, name: &str) -> Option<&VarInfo> {
        self.scopes.last()?.get(name)
    }

    pub fn lookup_var(&self, name: &str) -> Option<&VarInfo> {
        for scope in self.scopes.iter().rev() {
            if let Some(info) = scope.get(name) {
                return Some(info);
            }
        }
        None
    }

    pub fn define_func(&mut self, name: String, info: FuncInfo) {
        self.functions
            .insert(FunctionId::free_from_source_name(&name), info);
    }

    pub fn lookup_func(&self, name: &str) -> Option<&FuncInfo> {
        self.functions.get(&FunctionId::free_from_source_name(name))
    }

    pub fn define_class(&mut self, name: String, info: ClassInfo) {
        self.classes.insert(TypeId::from_source_name(&name), info);
    }

    pub fn lookup_class(&self, name: &str) -> Option<&ClassInfo> {
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

    pub fn lookup_module(&self, name: &str) -> Option<&ModuleInfo> {
        self.modules.get(self.module_names.get(name)?)
    }

    pub fn define_enum(&mut self, name: String, info: EnumInfo) {
        self.enums.insert(TypeId::from_source_name(&name), info);
    }

    pub fn lookup_enum(&self, name: &str) -> Option<&EnumInfo> {
        self.enums.get(&TypeId::from_source_name(name))
    }

    pub fn define_interface(&mut self, name: String, info: InterfaceInfo) {
        self.interfaces
            .insert(TypeId::from_source_name(&name), info);
    }

    pub fn lookup_interface(&self, name: &str) -> Option<&InterfaceInfo> {
        self.interfaces.get(&TypeId::from_source_name(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
