use crate::diagnostics::Span;
use crate::parser::ast::{ParamMode, Type};
use std::collections::HashMap;

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
    pub declaration_span: Span,
}

#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub params: Vec<Type>,
    pub param_infos: Vec<ParamInfo>,
    pub has_self: bool,
    pub return_type: Type,
    pub public: bool,
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
}
