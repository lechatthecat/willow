use crate::diagnostics::Span;
use crate::parser::ast::{ParamMode, Type};
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

fn substitute_type(
    ty: &Type,
    param_map: &std::collections::HashMap<String, Type>,
) -> Type {
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
        Type::Nullable(inner) => {
            Type::Nullable(Box::new(substitute_type(inner, param_map)))
        }
        Type::Array(inner) => {
            Type::Array(Box::new(substitute_type(inner, param_map)))
        }
        Type::Fn(params, ret) => Type::Fn(
            params.iter().map(|p| substitute_type(p, param_map)).collect(),
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

#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub params: Vec<Type>,
    pub param_infos: Vec<ParamInfo>,
    pub has_self: bool,
    pub return_type: Type,
    pub public: bool,
    pub protected: bool,
    pub is_open: bool,
    pub is_override: bool,
    pub declaration_span: Span,
}

#[derive(Debug, Clone)]
pub struct ClassInfo {
    pub name: String,
    pub public: bool,
    pub is_open: bool,
    pub base_class: Option<String>,
    pub declaration_span: Span,
    pub fields: HashMap<String, FieldInfo>,
    pub methods: HashMap<String, MethodInfo>,
}

/// Functions declared by an imported module.
#[derive(Debug, Default, Clone)]
pub struct ModuleInfo {
    pub functions: HashMap<String, FuncInfo>,
}

#[derive(Debug, Default)]
pub struct SymbolTable {
    scopes: Vec<HashMap<String, VarInfo>>,
    pub functions: HashMap<String, FuncInfo>,
    pub classes: HashMap<String, ClassInfo>,
    pub modules: HashMap<String, ModuleInfo>,
    pub enums: HashMap<String, EnumInfo>,
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
        self.functions.insert(name, info);
    }

    pub fn lookup_func(&self, name: &str) -> Option<&FuncInfo> {
        self.functions.get(name)
    }

    pub fn define_class(&mut self, name: String, info: ClassInfo) {
        self.classes.insert(name, info);
    }

    pub fn lookup_class(&self, name: &str) -> Option<&ClassInfo> {
        self.classes.get(name)
    }

    pub fn define_module(&mut self, name: String, info: ModuleInfo) {
        self.modules.insert(name, info);
    }

    pub fn lookup_module(&self, name: &str) -> Option<&ModuleInfo> {
        self.modules.get(name)
    }

    pub fn define_enum(&mut self, name: String, info: EnumInfo) {
        self.enums.insert(name, info);
    }

    pub fn lookup_enum(&self, name: &str) -> Option<&EnumInfo> {
        self.enums.get(name)
    }
}
