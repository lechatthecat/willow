use super::symbols::{
    ClassInfo, FieldInfo, FuncInfo, MethodInfo, ModuleInfo, ParamInfo, SymbolTable, VarInfo,
};
use crate::diagnostics::{Diagnostic, ErrorCode, FixSuggestion, Label, Severity, Span};
use crate::parser::ast::*;
use std::collections::{HashMap, HashSet};

pub struct TypeChecker {
    pub symbols: SymbolTable,
    pub errors: Vec<Diagnostic>,
    current_return_type: Type,
    /// Stack of lambda return types being inferred. When non-empty, `return` stmts
    /// record their type here instead of checking against `current_return_type`.
    lambda_return_stack: Vec<Option<Type>>,
    current_class: Option<String>,
    current_async_context: bool,
    narrowed_vars: Vec<HashMap<String, NarrowedVar>>,
}

#[derive(Clone)]
struct NarrowedVar {
    ty: Type,
    declaration_span: Span,
}

#[derive(Clone)]
struct NilCheckNarrowing {
    name: String,
    narrowed_ty: Type,
    declaration_span: Span,
    non_nil_when_true: bool,
}

impl TypeChecker {
    pub fn new() -> Self {
        let mut checker = Self {
            symbols: SymbolTable::default(),
            errors: Vec::new(),
            current_return_type: Type::Void,
            lambda_return_stack: Vec::new(),
            current_class: None,
            current_async_context: false,
            narrowed_vars: Vec::new(),
        };
        checker.register_builtin_functions();
        checker.register_builtin_modules();
        checker
    }

    fn register_builtin_functions(&mut self) {
        for name in ["pow", "powf"] {
            let params = vec![Type::F64, Type::F64];
            self.symbols.define_func(
                name.to_string(),
                FuncInfo {
                    param_infos: value_param_infos(&params),
                    params,
                    return_type: Type::F64,
                    public: true,
                    is_async: false,
                    declaration_span: Span::dummy(),
                    module_path: None,
                },
            );
        }
        self.symbols.define_func(
            "gc_collect".to_string(),
            FuncInfo {
                param_infos: vec![],
                params: vec![],
                return_type: Type::Void,
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
            },
        );
        self.symbols.define_func(
            "gc_allocated_bytes".to_string(),
            FuncInfo {
                param_infos: vec![],
                params: vec![],
                return_type: Type::I64,
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
            },
        );
    }

    fn register_builtin_modules(&mut self) {
        let mut env_functions = std::collections::HashMap::new();
        env_functions.insert(
            "args_len".to_string(),
            FuncInfo {
                param_infos: vec![],
                params: vec![],
                return_type: Type::I64,
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
            },
        );
        let arg_params = vec![Type::I64];
        env_functions.insert(
            "arg".to_string(),
            FuncInfo {
                param_infos: value_param_infos(&arg_params),
                params: arg_params,
                return_type: Type::String,
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
            },
        );
        env_functions.insert(
            "program_name".to_string(),
            FuncInfo {
                param_infos: vec![],
                params: vec![],
                return_type: Type::String,
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
            },
        );
        self.symbols.define_module(
            "env".to_string(),
            ModuleInfo {
                functions: env_functions,
            },
        );
    }

    /// Register an imported module's items so cross-module calls can report
    /// missing and private-item diagnostics accurately.
    pub fn register_module(&mut self, name: &str, path: &str, program: &Program) {
        let mut functions = HashMap::new();
        for item in &program.items {
            match item {
                Item::Function(f) => {
                    let params = f.params.iter().map(|p| p.ty.clone()).collect::<Vec<_>>();
                    functions.insert(
                        f.name.clone(),
                        FuncInfo {
                            param_infos: param_infos_from_decl(&f.params, None),
                            params,
                            return_type: f.return_type.clone(),
                            public: f.public,
                            is_async: f.is_async,
                            declaration_span: f.span,
                            module_path: Some(path.to_string()),
                        },
                    );
                }
                Item::Class(c) => {
                    let class_name = format!("{name}::{}", c.name);
                    self.symbols.define_class(
                        class_name.clone(),
                        class_info_from_decl(c, &class_name, Some(name)),
                    );
                }
            }
        }
        self.symbols
            .define_module(name.to_string(), ModuleInfo { functions });
    }

    pub fn check_program(&mut self, program: &Program) {
        // Pass 1: register class shapes (so methods can refer to other classes)
        for item in &program.items {
            if let Item::Class(c) = item {
                self.register_class(c);
            }
        }

        // Pass 2: register all top-level function signatures
        for item in &program.items {
            if let Item::Function(f) = item {
                let params: Vec<Type> = f.params.iter().map(|p| p.ty.clone()).collect();
                self.symbols.define_func(
                    f.name.clone(),
                    FuncInfo {
                        param_infos: param_infos_from_decl(&f.params, None),
                        params,
                        return_type: f.return_type.clone(),
                        public: f.public,
                        is_async: f.is_async,
                        declaration_span: f.span,
                        module_path: None,
                    },
                );
            }
        }

        // Pass 3: check bodies
        for item in &program.items {
            match item {
                Item::Function(f) => self.check_function(f),
                Item::Class(c) => self.check_class(c),
            }
        }
    }

    fn register_class(&mut self, c: &ClassDecl) {
        self.symbols
            .define_class(c.name.clone(), class_info_from_decl(c, &c.name, None));
    }

    fn check_class(&mut self, c: &ClassDecl) {
        self.check_class_inheritance(c);
        for field in &c.fields {
            self.validate_type(&field.ty, field.span);
        }
        for m in &c.methods {
            self.check_method(m, &c.name);
        }
    }

    fn check_class_inheritance(&mut self, c: &ClassDecl) {
        let Some(base_name) = c.base_class.as_ref().map(type_path_name) else {
            for method in &c.methods {
                if method.is_override {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0702,
                            format!(
                                "method `{}` is marked `override`, but `{}` has no base class",
                                method.name, c.name
                            ),
                        )
                        .with_label(Label::primary(method.span, "nothing to override"))
                        .with_help("remove `override` or add a base class with a matching method"),
                    );
                }
            }
            return;
        };

        match self.symbols.lookup_class(&base_name).cloned() {
            None => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0350,
                        format!("base class `{}` not found", base_name),
                    )
                    .with_label(Label::primary(c.span, "unknown base class")),
                );
                return;
            }
            Some(base) => {
                if !base.is_open {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0701,
                            format!("class `{}` is not open for inheritance", base_name),
                        )
                        .with_label(Label::primary(c.span, "cannot extend this class"))
                        .with_label(Label::secondary(
                            base.declaration_span,
                            "base class defined here",
                        ))
                        .with_help(format!(
                            "declare the base class as `open class {}`",
                            base.name
                        )),
                    );
                }
            }
        }

        for method in &c.methods {
            let inherited = self.lookup_method_in_ancestors(&base_name, &method.name);
            match (method.is_override, inherited) {
                (false, Some((owner, _))) => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0702,
                            format!(
                                "method `{}` overrides `{}` but is missing `override`",
                                method.name, owner
                            ),
                        )
                        .with_label(Label::primary(method.span, "missing `override`"))
                        .with_help(format!("write `override fn {}`", method.name)),
                    );
                }
                (true, None) => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0702,
                            format!(
                                "method `{}` is marked `override`, but no inherited method exists",
                                method.name
                            ),
                        )
                        .with_label(Label::primary(method.span, "no matching base method"))
                        .with_help("remove `override` or add a matching method to the base class"),
                    );
                }
                (true, Some((owner, base_method))) => {
                    if !base_method.is_open {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0703,
                                format!(
                                    "method `{}` in `{}` is not open for override",
                                    method.name, owner
                                ),
                            )
                            .with_label(Label::primary(method.span, "cannot override"))
                            .with_label(Label::secondary(
                                base_method.declaration_span,
                                "base method defined here",
                            ))
                            .with_help(format!(
                                "declare the base method as `open fn {}`",
                                method.name
                            )),
                        );
                    }

                    let method_params = method
                        .params
                        .iter()
                        .map(|param| param.ty.clone())
                        .collect::<Vec<_>>();
                    if method_params != base_method.params
                        || method.return_type != base_method.return_type
                    {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0703,
                                format!(
                                    "override `{}` does not match the inherited method signature",
                                    method.name
                                ),
                            )
                            .with_label(Label::primary(method.span, "signature mismatch"))
                            .with_label(Label::secondary(
                                base_method.declaration_span,
                                "inherited signature defined here",
                            ))
                            .with_help(
                                "use the same parameter and return types as the base method",
                            ),
                        );
                    }
                }
                (false, None) => {}
            }
        }
    }

    fn check_method(&mut self, m: &MethodDecl, class_name: &str) {
        self.validate_type(&m.return_type, m.span);
        for param in &m.params {
            self.validate_type(&param.ty, param.span);
        }
        if m.is_async {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0807,
                    "async methods are not supported yet",
                )
                .with_label(Label::primary(m.span, "async method parsed here"))
                .with_help("async lowering and runtime support are tracked separately"),
            );
        }
        let previous_class = self.current_class.replace(class_name.to_string());
        let previous_async_context = self.current_async_context;
        self.current_async_context = m.is_async;
        self.current_return_type = m.return_type.clone();
        self.symbols.push_scope();

        // `self` has the type of the enclosing class
        if m.has_self {
            self.symbols.define_var(
                "self".to_string(),
                VarInfo {
                    ty: Type::Named(class_name.to_string()),
                    mutable: false,
                    is_param: true,
                    declaration_span: m.span,
                },
            );
        }

        for param in &m.params {
            self.symbols.define_var(
                param.name.clone(),
                VarInfo {
                    ty: param.ty.clone(),
                    mutable: matches!(&param.mode, ParamMode::Inout { .. }),
                    is_param: true,
                    declaration_span: param.span,
                },
            );
        }

        self.check_block(&m.body);
        self.symbols.pop_scope();
        self.current_class = previous_class;
        self.current_async_context = previous_async_context;
    }

    fn check_function(&mut self, f: &FunctionDecl) {
        self.validate_type(&f.return_type, f.span);
        for param in &f.params {
            self.validate_type(&param.ty, param.span);
        }
        if f.is_async {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0807,
                    "async functions are not supported yet",
                )
                .with_label(Label::primary(f.span, "async function parsed here"))
                .with_help("async lowering and runtime support are tracked separately"),
            );
        }
        let previous_async_context = self.current_async_context;
        self.current_async_context = f.is_async;
        self.current_return_type = f.return_type.clone();
        self.symbols.push_scope();
        for param in &f.params {
            self.symbols.define_var(
                param.name.clone(),
                VarInfo {
                    ty: param.ty.clone(),
                    mutable: matches!(&param.mode, ParamMode::Inout { .. }),
                    is_param: true,
                    declaration_span: param.span,
                },
            );
        }
        self.check_block(&f.body);
        self.symbols.pop_scope();
        self.current_async_context = previous_async_context;
    }

    fn check_block(&mut self, block: &Block) {
        self.symbols.push_scope();
        self.narrowed_vars.push(HashMap::new());
        for stmt in &block.stmts {
            self.check_stmt(stmt);
        }
        self.narrowed_vars.pop();
        self.symbols.pop_scope();
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => {
                let inferred = self.check_expr(&s.init);
                let ty = if let Some(ann) = &s.ty {
                    self.validate_type(ann, s.span);
                    if !self.types_compatible(ann, &inferred) {
                        let code = self.type_mismatch_error_code(ann, &inferred);
                        let message = if code == ErrorCode::E0704 {
                            format!(
                                "cannot assign `{}` to variable `{}` of type `{}`",
                                type_name(&inferred),
                                s.name,
                                type_name(ann)
                            )
                        } else {
                            format!(
                                "mismatched types: expected `{}`, found `{}`",
                                type_name(ann),
                                type_name(&inferred)
                            )
                        };
                        let label = if code == ErrorCode::E0704 {
                            format!(
                                "expected `{}` because of this type annotation",
                                type_name(ann)
                            )
                        } else {
                            format!("expected `{}`", type_name(ann))
                        };
                        self.push(
                            Diagnostic::new(Severity::Error, code, message)
                                .with_label(Label::primary(s.span, label)),
                        );
                    }
                    ann.clone()
                } else {
                    if inferred == Type::Nil {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                "cannot infer the type of `nil`",
                            )
                            .with_label(Label::primary(
                                s.init.span(),
                                "`nil` needs a nullable type",
                            ))
                            .with_help(
                                "add a nullable type annotation, e.g. `let value: Node? = nil;`",
                            ),
                        );
                    }
                    inferred
                };
                // E0351: reject redeclaration in the same scope.
                if let Some(_prev) = self.symbols.lookup_var_current_scope(&s.name) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0351,
                            format!("variable `{}` is already defined in this scope", s.name),
                        )
                        .with_label(Label::primary(s.span, "previous definition here")),
                    );
                }
                self.symbols.define_var(
                    s.name.clone(),
                    VarInfo {
                        ty,
                        mutable: s.mutable,
                        is_param: false,
                        declaration_span: s.span,
                    },
                );
            }
            Stmt::Assign(s) => {
                let info = self.symbols.lookup_var(&s.name).cloned();
                match info {
                    None => self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0350,
                            format!("cannot find variable `{}`", s.name),
                        )
                        .with_label(Label::primary(s.span, "not found in this scope")),
                    ),
                    Some(info) => {
                        if !info.mutable {
                            if info.is_param {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0302,
                                        format!(
                                            "cannot assign to immutable parameter `{}`",
                                            s.name
                                        ),
                                    )
                                    .with_label(Label::primary(
                                        s.span,
                                        "cannot assign to parameter",
                                    ))
                                    .with_help(format!(
                                        "introduce a mutable local variable: `let mut {} = {};`",
                                        s.name, s.name
                                    )),
                                );
                            } else {
                                // Build an insertion span just after "let " in the declaration.
                                let decl = info.declaration_span;
                                let insert_span = Span::new(
                                    decl.start + 4,
                                    decl.start + 4,
                                    decl.line,
                                    decl.col + 4,
                                );
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0301,
                                        format!("cannot assign to immutable variable `{}`", s.name),
                                    )
                                    .with_label(Label::primary(s.span, "cannot assign"))
                                    .with_label(Label::secondary(
                                        info.declaration_span,
                                        "declared immutable here",
                                    ))
                                    .with_help(format!(
                                        "declare it as mutable: `let mut {} = ...`",
                                        s.name
                                    ))
                                    .with_fix(
                                        FixSuggestion::insertion(
                                            insert_span,
                                            "mut ",
                                            "add `mut` here",
                                        ),
                                    ),
                                );
                            }
                        }
                        let got = self.check_expr(&s.value);
                        if !self.types_compatible(&info.ty, &got) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    self.type_mismatch_error_code(&info.ty, &got),
                                    format!(
                                        "mismatched types: expected `{}`, found `{}`",
                                        type_name(&info.ty),
                                        type_name(&got)
                                    ),
                                )
                                .with_label(Label::primary(
                                    s.span,
                                    format!("expected `{}`", type_name(&info.ty)),
                                )),
                            );
                        }
                        self.clear_narrowing(&s.name);
                    }
                }
            }
            Stmt::If(s) => {
                let cond_ty = self.check_expr(&s.cond);
                let nil_narrowing = self.nil_check_narrowing(&s.cond);
                if cond_ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0203,
                            format!("condition must be `bool`, found `{}`", type_name(&cond_ty)),
                        )
                        .with_label(Label::primary(
                            s.cond.span(),
                            format!("expected `bool`, found `{}`", type_name(&cond_ty)),
                        ))
                        .with_help("use an explicit comparison, e.g. `!= 0`"),
                    );
                }
                match nil_narrowing.as_ref() {
                    Some(narrowing) if narrowing.non_nil_when_true => {
                        self.check_block_with_narrowing(&s.then_block, narrowing);
                    }
                    _ => self.check_block(&s.then_block),
                }
                if let Some(else_b) = &s.else_block {
                    match nil_narrowing.as_ref() {
                        Some(narrowing) if !narrowing.non_nil_when_true => {
                            self.check_block_with_narrowing(else_b, narrowing);
                        }
                        _ => self.check_block(else_b),
                    }
                } else if let Some(narrowing) = nil_narrowing.as_ref() {
                    if !narrowing.non_nil_when_true && block_always_returns(&s.then_block) {
                        self.add_narrowing_to_current_scope(narrowing);
                    }
                }
            }
            Stmt::While(s) => {
                let cond_ty = self.check_expr(&s.cond);
                if cond_ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0203,
                            format!("condition must be `bool`, found `{}`", type_name(&cond_ty)),
                        )
                        .with_label(Label::primary(
                            s.cond.span(),
                            format!("expected `bool`, found `{}`", type_name(&cond_ty)),
                        ))
                        .with_help("use an explicit comparison, e.g. `!= 0`"),
                    );
                }
                self.check_block(&s.body);
            }
            Stmt::Return(s) => {
                let ret_ty = s
                    .value
                    .as_ref()
                    .map(|v| self.check_expr(v))
                    .unwrap_or(Type::Void);
                // Inside a lambda with no annotation: record the return type for inference.
                if let Some(slot) = self.lambda_return_stack.last_mut() {
                    if slot.is_none() {
                        *slot = Some(ret_ty.clone());
                    }
                    return; // don't validate against outer current_return_type
                }
                if !self.types_compatible(&self.current_return_type, &ret_ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(&self.current_return_type, &ret_ty),
                            format!(
                                "mismatched types: expected `{}`, found `{}`",
                                type_name(&self.current_return_type),
                                type_name(&ret_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            s.span,
                            format!("expected `{}`", type_name(&self.current_return_type)),
                        )),
                    );
                }
            }
            Stmt::Expr(s) => {
                self.check_expr(&s.expr);
            }
        }
    }

    fn check_expr(&mut self, expr: &Expr) -> Type {
        match expr {
            Expr::Integer(_, _) => Type::I64,
            Expr::Float(_, _) => Type::F64,
            Expr::Bool(_, _) => Type::Bool,
            Expr::Nil(_) => Type::Nil,
            Expr::String(_, _) => Type::String,
            Expr::Var(name, span) => {
                // Local variable?
                if let Some(info) = self.symbols.lookup_var(name) {
                    if let Some(narrowed_ty) = self.lookup_narrowed_type(name) {
                        return narrowed_ty;
                    }
                    return info.ty.clone();
                }
                // Named function used as a value: `apply(10, double)` where `double: fn(...)`
                if let Some(info) = self.symbols.lookup_func(name) {
                    let params = info.params.clone();
                    let ret = info.return_type.clone();
                    return Type::Fn(params, Box::new(ret));
                }
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0350,
                        format!("cannot find variable `{}`", name),
                    )
                    .with_label(Label::primary(*span, "not found in this scope")),
                );
                Type::I64
            }
            Expr::Binary(b) => self.check_binary(b),
            Expr::Unary(u) => self.check_unary(u),
            Expr::Call(c) => {
                if c.callee == "format" {
                    return self.check_format_call(c);
                }

                // Direct call to a named function.
                if let Some(info) = self.symbols.lookup_func(&c.callee).cloned() {
                    if info.params.len() != c.args.len() {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "function `{}` takes {} argument(s) but {} were supplied",
                                    c.callee,
                                    info.params.len(),
                                    c.args.len()
                                ),
                            )
                            .with_label(Label::primary(c.span, "wrong number of arguments")),
                        );
                    }
                    self.check_call_args_against_param_infos(&info.param_infos, &c.args);
                    return function_call_return_type(&info);
                }

                // Indirect call through a function-type local variable.
                if let Some(var_info) = self.symbols.lookup_var(&c.callee).cloned() {
                    if let Type::Fn(param_types, ret) = var_info.ty {
                        if param_types.len() != c.args.len() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "function value `{}` takes {} argument(s) but {} were supplied",
                                        c.callee,
                                        param_types.len(),
                                        c.args.len()
                                    ),
                                )
                                .with_label(Label::primary(c.span, "wrong number of arguments")),
                            );
                        }
                        self.check_value_call_args(&param_types, &c.args);
                        return *ret;
                    }
                }

                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0350,
                        format!("cannot find function `{}`", c.callee),
                    )
                    .with_label(Label::primary(c.span, "not found in this scope")),
                );
                Type::Void
            }
            Expr::FieldAccess(obj, field_name, span) => {
                let obj_ty = self.check_expr(obj);
                self.resolve_field(&obj_ty, field_name, *span, true)
            }
            Expr::MethodCall(m) => {
                let obj_ty = self.check_expr(&m.object);
                if let Some(ret) = self.check_concurrency_method_call(&obj_ty, m) {
                    return ret;
                }
                let ret = self.resolve_method(&obj_ty, &m.method, &m.args, m.span);
                ret
            }
            Expr::StaticCall(s) => self.resolve_static_call(&s.class, &s.method, &s.args, s.span),
            Expr::ObjectLiteral(o) => self.check_object_literal(o),
            Expr::Spawn(s) => self.check_spawn(s),
            Expr::Await(a) => {
                let awaited_ty = self.check_expr(&a.expr);
                if !self.current_async_context {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0801,
                            "`await` can only be used inside an async function",
                        )
                        .with_label(Label::primary(
                            a.span,
                            "`await` used in a non-async function",
                        ))
                        .with_help("make the enclosing function `async`"),
                    );
                    return Type::Void;
                }
                match awaited_ty {
                    Type::Generic(name, mut args) if name == "Future" && args.len() == 1 => {
                        args.remove(0)
                    }
                    other => {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0803,
                                format!("cannot await value of type `{}`", type_name(&other)),
                            )
                            .with_label(Label::primary(a.expr.span(), "expected `Future<T>`"))
                            .with_help(
                                "await only values returned by async functions or Future APIs",
                            ),
                        );
                        Type::Void
                    }
                }
            }
            Expr::Select(s) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0807,
                        "select blocks are not supported yet",
                    )
                    .with_label(Label::primary(s.span, "select block parsed here"))
                    .with_help("select lowering and async channel support are tracked separately"),
                );
                Type::Void
            }
            Expr::Print(arg, _, _) => {
                self.check_expr(arg);
                Type::Void
            }
            Expr::Ternary(t) => {
                let cond_ty = self.check_expr(&t.condition);
                if cond_ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0901,
                            format!(
                                "ternary condition must be `bool`, found `{}`",
                                type_name(&cond_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            t.condition.span(),
                            format!("expected `bool`, found `{}`", type_name(&cond_ty)),
                        )),
                    );
                }
                let then_ty = self.check_expr(&t.then_expr);
                let else_ty = self.check_expr(&t.else_expr);
                if let Some(unified_ty) = self.unify_ternary_types(&then_ty, &else_ty) {
                    self.validate_type(&unified_ty, t.span);
                    unified_ty
                } else {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0902,
                            format!(
                                "ternary branches have incompatible types: `{}` and `{}`",
                                type_name(&then_ty),
                                type_name(&else_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            t.else_expr.span(),
                            format!(
                                "expected `{}`, found `{}`",
                                type_name(&then_ty),
                                type_name(&else_ty)
                            ),
                        ))
                        .with_label(Label::secondary(
                            t.then_expr.span(),
                            format!("this branch has type `{}`", type_name(&then_ty)),
                        )),
                    );
                    Type::Void
                }
            }
            Expr::Lambda(l) => self.check_lambda(l),
        }
    }

    fn check_lambda(&mut self, l: &LambdaExpr) -> Type {
        // All params must have type annotations (or infer from expected type — not yet supported).
        let mut param_types = Vec::new();
        for p in &l.params {
            match &p.ty {
                Some(ty) => {
                    self.validate_type(ty, p.span);
                    param_types.push(ty.clone());
                }
                None => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E1001,
                            format!("cannot infer type for lambda parameter `{}`", p.name),
                        )
                        .with_label(Label::primary(p.span, "type annotation required"))
                        .with_help("add a parameter type, e.g. `|x: i64|`"),
                    );
                    param_types.push(Type::I64); // recover
                }
            }
        }

        // Determine expected return type from annotation (if any) for use in the body.
        let expected_ret = l.return_type.clone();
        if let Some(ret) = &expected_ret {
            self.validate_type(ret, l.span);
        }

        // Type-check the body with params in scope.
        self.symbols.push_scope();
        for (p, ty) in l.params.iter().zip(&param_types) {
            self.symbols.define_var(
                p.name.clone(),
                crate::semantic::symbols::VarInfo {
                    ty: ty.clone(),
                    mutable: false,
                    is_param: true,
                    declaration_span: p.span,
                },
            );
        }

        // Save/restore outer return type so `return` stmts in the lambda body
        // are checked against the lambda's return type, not the enclosing function's.
        let saved_ret_ty = self.current_return_type.clone();

        let body_ty = match &l.body {
            LambdaBody::Expr(e) => self.check_expr(e),
            LambdaBody::Block(b) => {
                if let Some(ref ann) = expected_ret {
                    // Annotation provided: validate return stmts against it.
                    self.current_return_type = ann.clone();
                    for stmt in &b.stmts {
                        self.check_stmt(stmt);
                    }
                    ann.clone()
                } else {
                    // No annotation: collect the return type via the lambda stack.
                    self.lambda_return_stack.push(None);
                    for stmt in &b.stmts {
                        self.check_stmt(stmt);
                    }
                    let inferred = self
                        .lambda_return_stack
                        .pop()
                        .flatten()
                        .unwrap_or(Type::Void);
                    inferred
                }
            }
        };
        self.current_return_type = saved_ret_ty;
        self.symbols.pop_scope();

        let ret_ty = match &l.return_type {
            Some(ann) => {
                if !self.types_compatible(ann, &body_ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(ann, &body_ty),
                            format!(
                                "lambda return type mismatch: expected `{}`, found `{}`",
                                type_name(ann),
                                type_name(&body_ty)
                            ),
                        )
                        .with_label(Label::primary(l.span, "return type mismatch")),
                    );
                }
                ann.clone()
            }
            None => body_ty,
        };

        Type::Fn(param_types, Box::new(ret_ty))
    }

    fn check_spawn(&mut self, spawn: &SpawnExpr) -> Type {
        if let Some(info) = self.symbols.lookup_func(&spawn.callee).cloned() {
            self.check_call_argument_count(
                &format!("spawn target `{}`", spawn.callee),
                info.params.len(),
                spawn.args.len(),
                spawn.span,
            );
            self.check_call_args_against_param_infos(&info.param_infos, &spawn.args);
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0807,
                    "spawn lowering is not supported yet",
                )
                .with_label(Label::primary(spawn.span, "spawn parsed here"))
                .with_help("task runtime and JoinHandle lowering are tracked separately"),
            );
            return Type::Generic(
                "JoinHandle".to_string(),
                vec![function_call_return_type(&info)],
            );
        }

        if let Some(var_info) = self.symbols.lookup_var(&spawn.callee).cloned() {
            if let Type::Fn(params, ret) = var_info.ty {
                self.check_call_arguments(
                    &format!("spawn target `{}`", spawn.callee),
                    &params,
                    &spawn.args,
                    spawn.span,
                );
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0807,
                        "spawn lowering is not supported yet",
                    )
                    .with_label(Label::primary(spawn.span, "spawn parsed here"))
                    .with_help("task runtime and JoinHandle lowering are tracked separately"),
                );
                return Type::Generic("JoinHandle".to_string(), vec![*ret]);
            }
        }

        for arg in &spawn.args {
            self.check_expr(&arg.expr);
        }
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0804,
                format!("spawn target `{}` is not callable", spawn.callee),
            )
            .with_label(Label::primary(
                spawn.span,
                "not a function or function value",
            ))
            .with_help("spawn a named function call, e.g. `spawn work(10)`"),
        );
        Type::Void
    }

    fn check_concurrency_method_call(
        &mut self,
        obj_ty: &Type,
        call: &MethodCallExpr,
    ) -> Option<Type> {
        match call.method.as_str() {
            "join" => {
                if !call.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("join expects 0 arguments, got {}", call.args.len()),
                        )
                        .with_label(Label::primary(call.span, "wrong number of arguments")),
                    );
                }
                match obj_ty {
                    Type::Generic(name, args) if name == "JoinHandle" && args.len() == 1 => {
                        Some(args[0].clone())
                    }
                    _ => {
                        for arg in &call.args {
                            self.check_expr(&arg.expr);
                        }
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0805,
                                format!("cannot call `join` on `{}`", type_name(obj_ty)),
                            )
                            .with_label(Label::primary(call.span, "expected `JoinHandle<T>`")),
                        );
                        Some(Type::Void)
                    }
                }
            }
            "send" => {
                let channel_type = channel_element_type(obj_ty);
                if channel_type.is_none() {
                    for arg in &call.args {
                        self.check_expr(&arg.expr);
                    }
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0806,
                            format!("cannot call `send` on `{}`", type_name(obj_ty)),
                        )
                        .with_label(Label::primary(call.span, "expected `Channel<T>`")),
                    );
                    return Some(Type::Void);
                }
                let element_ty = channel_type.unwrap();
                if call.args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("send expects 1 argument, got {}", call.args.len()),
                        )
                        .with_label(Label::primary(call.span, "wrong number of arguments")),
                    );
                }
                if let Some(arg) = call.args.first() {
                    let arg_ty = self.check_expr(&arg.expr);
                    if !self.types_compatible(&element_ty, &arg_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0802,
                                format!(
                                    "cannot send `{}` into `Channel<{}>`",
                                    type_name(&arg_ty),
                                    type_name(&element_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                arg.expr.span(),
                                format!(
                                    "expected `{}`, found `{}`",
                                    type_name(&element_ty),
                                    type_name(&arg_ty)
                                ),
                            )),
                        );
                    }
                }
                Some(Type::Void)
            }
            "recv" => {
                if !call.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("recv expects 0 arguments, got {}", call.args.len()),
                        )
                        .with_label(Label::primary(call.span, "wrong number of arguments")),
                    );
                }
                match channel_element_type(obj_ty) {
                    Some(element_ty) => Some(element_ty),
                    None => {
                        for arg in &call.args {
                            self.check_expr(&arg.expr);
                        }
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0806,
                                format!("cannot call `recv` on `{}`", type_name(obj_ty)),
                            )
                            .with_label(Label::primary(call.span, "expected `Channel<T>`")),
                        );
                        Some(Type::Void)
                    }
                }
            }
            "close" => {
                if !call.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("close expects 0 arguments, got {}", call.args.len()),
                        )
                        .with_label(Label::primary(call.span, "wrong number of arguments")),
                    );
                }
                if channel_element_type(obj_ty).is_none() {
                    for arg in &call.args {
                        self.check_expr(&arg.expr);
                    }
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0806,
                            format!("cannot call `close` on `{}`", type_name(obj_ty)),
                        )
                        .with_label(Label::primary(call.span, "expected `Channel<T>`")),
                    );
                }
                Some(Type::Void)
            }
            _ => None,
        }
    }

    fn check_call_arguments(
        &mut self,
        callee: &str,
        params: &[Type],
        args: &[CallArg],
        span: Span,
    ) {
        self.check_call_argument_count(callee, params.len(), args.len(), span);
        self.check_value_call_args(params, args);
    }

    fn check_call_argument_count(
        &mut self,
        callee: &str,
        expected: usize,
        supplied: usize,
        span: Span,
    ) {
        if expected != supplied {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "{} takes {} argument(s) but {} were supplied",
                        callee, expected, supplied
                    ),
                )
                .with_label(Label::primary(span, "wrong number of arguments")),
            );
        }
    }

    fn check_value_call_args(&mut self, params: &[Type], args: &[CallArg]) {
        let param_infos = value_param_infos(params);
        self.check_call_args_against_param_infos(&param_infos, args);
    }

    fn check_call_args_against_param_infos(&mut self, params: &[ParamInfo], args: &[CallArg]) {
        for (param, arg) in params.iter().zip(args) {
            self.check_call_arg_against_param(param, arg);
        }
    }

    fn check_call_arg_against_param(&mut self, param: &ParamInfo, arg: &CallArg) {
        match (&param.mode, &arg.mode) {
            (ParamMode::Value, CallArgMode::Value) => {
                self.check_value_arg_type(&param.ty, arg);
            }
            (ParamMode::Value, CallArgMode::Inout { .. }) => {
                let arg_ty = self.check_expr(&arg.expr);
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1703,
                        "unexpected reference argument",
                    )
                    .with_label(Label::primary(
                        arg.span,
                        format!(
                            "parameter expects `{}`, not `inout {}`",
                            type_name(&param.ty),
                            type_name(&arg_ty)
                        ),
                    )),
                );
            }
            (ParamMode::Inout { .. }, CallArgMode::Value) => {
                self.check_expr(&arg.expr);
                let expr_span = arg.expr.span();
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1702,
                    "expected reference argument for `inout` parameter",
                )
                .with_label(Label::primary(
                    expr_span,
                    "expected `&` before this argument",
                ))
                .with_help("pass the mutable place by reference");

                if let Expr::Var(name, _) = &arg.expr {
                    diagnostic = diagnostic.with_help(format!("write `&{}`", name));
                    diagnostic = diagnostic.with_fix(FixSuggestion::insertion(
                        Span::new(
                            expr_span.start,
                            expr_span.start,
                            expr_span.line,
                            expr_span.col,
                        ),
                        "&",
                        "pass the variable by reference",
                    ));
                }

                self.push(diagnostic);
            }
            (ParamMode::Inout { .. }, CallArgMode::Inout { .. }) => {
                self.check_inout_argument(param, arg);
            }
        }
    }

    fn check_value_arg_type(&mut self, param_ty: &Type, arg: &CallArg) {
        let arg_ty = self.check_expr(&arg.expr);
        if !self.types_compatible(param_ty, &arg_ty) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    self.type_mismatch_error_code(param_ty, &arg_ty),
                    format!(
                        "mismatched types: expected `{}`, found `{}`",
                        type_name(param_ty),
                        type_name(&arg_ty)
                    ),
                )
                .with_label(Label::primary(
                    arg.expr.span(),
                    format!("expected `{}`", type_name(param_ty)),
                )),
            );
        }
    }

    fn check_inout_argument(&mut self, param: &ParamInfo, arg: &CallArg) {
        let Expr::Var(name, _) = &arg.expr else {
            self.check_expr(&arg.expr);
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E1704,
                "cannot pass non-place expression as `inout`",
            )
            .with_label(Label::primary(arg.span, "not an assignable place"));

            if matches!(&arg.expr, Expr::Call(_)) {
                diagnostic = diagnostic.with_help("function call results are temporaries");
            }

            self.push(diagnostic);
            return;
        };

        let Some(var_info) = self.symbols.lookup_var(name).cloned() else {
            self.check_expr(&arg.expr);
            return;
        };

        if !var_info.mutable {
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E1701,
                format!("cannot pass immutable variable `{}` as `inout`", name),
            )
            .with_label(Label::primary(
                arg.span,
                "cannot pass immutable variable by reference",
            ))
            .with_label(Label::secondary(
                var_info.declaration_span,
                "declared immutable here",
            ))
            .with_help("declare the variable as mutable");

            if !var_info.is_param {
                let decl = var_info.declaration_span;
                let insert_span =
                    Span::new(decl.start + 4, decl.start + 4, decl.line, decl.col + 4);
                diagnostic = diagnostic.with_fix(FixSuggestion::insertion(
                    insert_span,
                    "mut ",
                    "add `mut` here",
                ));
            }

            self.push(diagnostic);
        }

        if var_info.ty != param.ty {
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E1705,
                "`inout` argument type mismatch",
            )
            .with_label(Label::primary(
                arg.span,
                format!("found `{}`", type_name(&var_info.ty)),
            ));

            if param.type_span != Span::dummy() {
                diagnostic = diagnostic.with_label(Label::secondary(
                    param.type_span,
                    format!("expected `{}`", type_name(&param.ty)),
                ));
            } else {
                diagnostic = diagnostic.with_label(Label::secondary(
                    param.span,
                    format!("expected `{}`", type_name(&param.ty)),
                ));
            }

            self.push(diagnostic);
        }
    }

    fn check_format_call(&mut self, c: &CallExpr) -> Type {
        if c.args.len() != 2 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!("format expects 2 arguments, got {}", c.args.len()),
                )
                .with_label(Label::primary(c.span, "wrong number of arguments")),
            );
            for arg in &c.args {
                self.check_expr(&arg.expr);
            }
            return Type::String;
        }

        match &c.args[0].expr {
            Expr::String(spec, span) if is_supported_f64_format(spec) => {
                let _ = span;
            }
            Expr::String(spec, span) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1401,
                        format!("invalid format specifier `{}`", spec),
                    )
                    .with_label(Label::primary(*span, "unsupported format specifier"))
                    .with_help("supported f64 formats are `{:.17g}`, `{:.16f}`, and `{:.6f}`"),
                );
            }
            other => {
                self.check_expr(other);
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1401,
                        "format specifier must be a string literal",
                    )
                    .with_label(Label::primary(other.span(), "expected string literal"))
                    .with_help("write the format as a literal, e.g. `format(\"{:.6f}\", value)`"),
                );
            }
        }

        let value_ty = self.check_expr(&c.args[1].expr);
        if value_ty != Type::F64 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "mismatched types: expected `f64`, found `{}`",
                        type_name(&value_ty)
                    ),
                )
                .with_label(Label::primary(c.args[1].expr.span(), "expected `f64`")),
            );
        }

        Type::String
    }

    fn check_object_literal(&mut self, literal: &ObjectLiteralExpr) -> Type {
        let class = match self.symbols.lookup_class(&literal.class).cloned() {
            Some(class) => class,
            None => {
                for field in &literal.fields {
                    self.check_expr(&field.value);
                }
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0350,
                        format!("class `{}` not found", literal.class),
                    )
                    .with_label(Label::primary(literal.span, "unknown class")),
                );
                return Type::Void;
            }
        };

        let mut seen = HashSet::new();
        for field in &literal.fields {
            let value_ty = self.check_expr(&field.value);
            if !seen.insert(field.name.clone()) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0502,
                        format!("field `{}` is initialized more than once", field.name),
                    )
                    .with_label(Label::primary(field.span, "duplicate field initializer")),
                );
                continue;
            }

            match class.fields.get(&field.name) {
                Some(info) => {
                    if !self.types_compatible(&info.ty, &value_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                self.type_mismatch_error_code(&info.ty, &value_ty),
                                format!(
                                    "field `{}` expects `{}`, found `{}`",
                                    field.name,
                                    type_name(&info.ty),
                                    type_name(&value_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                field.value.span(),
                                format!("expected `{}`", type_name(&info.ty)),
                            ))
                            .with_label(Label::secondary(
                                info.declaration_span,
                                "field declared here",
                            )),
                        );
                    }
                }
                None => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0502,
                            format!("no field `{}` on class `{}`", field.name, literal.class),
                        )
                        .with_label(Label::primary(field.span, "unknown field")),
                    );
                }
            }
        }

        for (field_name, field_info) in &class.fields {
            if !seen.contains(field_name) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0502,
                        format!(
                            "missing field `{}` in `{}` literal",
                            field_name, literal.class
                        ),
                    )
                    .with_label(Label::primary(literal.span, "missing field initializer"))
                    .with_label(Label::secondary(
                        field_info.declaration_span,
                        "field declared here",
                    )),
                );
            }
        }

        Type::Named(literal.class.clone())
    }

    fn check_binary(&mut self, b: &BinaryExpr) -> Type {
        let lty = self.check_expr(&b.lhs);
        let rty = self.check_expr(&b.rhs);

        match &b.op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                if b.op == BinOp::Add && lty == Type::String && rty == Type::String {
                    return Type::String;
                }

                if (lty != Type::I64 && lty != Type::F64) || lty != rty {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!(
                                "cannot apply operator `{}` to `{}` and `{}`",
                                b.op.symbol(),
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )
                        .with_label(Label::primary(
                            b.span,
                            format!(
                                "`{}` not defined for `{}` and `{}`",
                                b.op.symbol(),
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )),
                    );
                }
                lty
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                if (lty != Type::I64 && lty != Type::F64) || lty != rty {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!(
                                "cannot compare `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )
                        .with_label(Label::primary(
                            b.span,
                            format!(
                                "comparison not defined for `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )),
                    );
                }
                Type::Bool
            }
            BinOp::Eq | BinOp::Ne => {
                if lty == Type::Nil || rty == Type::Nil {
                    self.check_nil_comparison(&lty, &rty, b.span);
                    return Type::Bool;
                }

                if !self.types_compatible(&lty, &rty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "mismatched types: `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )
                        .with_label(Label::primary(
                            b.span,
                            format!(
                                "cannot compare `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )),
                    );
                }
                Type::Bool
            }
            BinOp::And | BinOp::Or => {
                if lty != Type::Bool || rty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!(
                                "logical operator requires `bool` operands, found `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )
                        .with_label(Label::primary(b.span, "operands must be `bool`")),
                    );
                }
                Type::Bool
            }
        }
    }

    fn check_unary(&mut self, u: &UnaryExpr) -> Type {
        let ty = self.check_expr(&u.expr);
        match &u.op {
            UnaryOp::Neg => {
                if ty != Type::I64 && ty != Type::F64 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!("unary `-` cannot be applied to `{}`", type_name(&ty)),
                        )
                        .with_label(Label::primary(
                            u.span,
                            format!("requires `i64` or `f64`, found `{}`", type_name(&ty)),
                        )),
                    );
                }
                ty
            }
            UnaryOp::Not => {
                if ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!("unary `!` cannot be applied to `{}`", type_name(&ty)),
                        )
                        .with_label(Label::primary(
                            u.span,
                            format!("requires `bool`, found `{}`", type_name(&ty)),
                        )),
                    );
                }
                Type::Bool
            }
        }
    }

    fn resolve_field(
        &mut self,
        obj_ty: &Type,
        field_name: &str,
        span: Span,
        check_visibility: bool,
    ) -> Type {
        let class_name = match obj_ty {
            Type::Named(n) => n.clone(),
            Type::Nullable(_) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "cannot access field `{}` on nullable type `{}`",
                            field_name,
                            type_name(obj_ty)
                        ),
                    )
                    .with_label(Label::primary(span, "nullable value may be `nil`"))
                    .with_help("check the value with `!= nil` before accessing fields"),
                );
                return Type::Void;
            }
            _ => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("type `{}` has no fields", type_name(obj_ty)),
                    )
                    .with_label(Label::primary(span, "field access on non-class type")),
                );
                return Type::Void;
            }
        };
        if self.symbols.lookup_class(&class_name).is_none() {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0350,
                    format!("class `{}` not found", class_name),
                )
                .with_label(Label::primary(span, "unknown class")),
            );
            return Type::Void;
        }
        match self.lookup_field_in_hierarchy(&class_name, field_name) {
            None => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0502,
                        format!("no field `{}` on class `{}`", field_name, class_name),
                    )
                    .with_label(Label::primary(span, "field not found")),
                );
                Type::Void
            }
            Some((owner, fi)) => {
                if check_visibility && !fi.public && !self.can_access_private_member(&owner) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0501,
                            format!("field `{}` of class `{}` is private", field_name, owner),
                        )
                        .with_label(Label::primary(span, "private field"))
                        .with_label(Label::secondary(fi.declaration_span, "field defined here"))
                        .with_help(format!(
                            "expose it using `pub {}: {}` or provide a public getter method",
                            field_name,
                            type_name(&fi.ty)
                        )),
                    );
                }
                fi.ty.clone()
            }
        }
    }

    fn resolve_method(
        &mut self,
        obj_ty: &Type,
        method_name: &str,
        args: &[CallArg],
        span: Span,
    ) -> Type {
        let class_name = match obj_ty {
            Type::Named(n) => n.clone(),
            Type::Nullable(_) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "cannot call method `{}` on nullable type `{}`",
                            method_name,
                            type_name(obj_ty)
                        ),
                    )
                    .with_label(Label::primary(span, "nullable value may be `nil`"))
                    .with_help("check the value with `!= nil` before calling methods"),
                );
                return Type::Void;
            }
            _ => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("type `{}` has no methods", type_name(obj_ty)),
                    )
                    .with_label(Label::primary(span, "method call on non-class type")),
                );
                return Type::Void;
            }
        };
        if self.symbols.lookup_class(&class_name).is_none() {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0350,
                    format!("class `{}` not found", class_name),
                )
                .with_label(Label::primary(span, "unknown class")),
            );
            return Type::Void;
        }
        match self.lookup_method_in_hierarchy(&class_name, method_name) {
            None => {
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0502,
                    format!("no method `{}` on class `{}`", method_name, class_name),
                )
                .with_label(Label::primary(span, "method not found"));

                let method_names = self.method_names_in_hierarchy(&class_name);
                if let Some(suggestion) = suggest_similar_name(method_name, method_names.iter()) {
                    diagnostic = diagnostic
                        .with_help(format!(
                            "there is a method with a similar name: `{}`",
                            suggestion
                        ))
                        .with_fix(FixSuggestion::new(
                            span,
                            suggestion,
                            "replace with suggested method",
                        ));
                }

                self.push(diagnostic);
                Type::Void
            }
            Some((owner, mi)) => {
                if !mi.public && !self.can_access_private_member(&owner) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0501,
                            format!("method `{}` of class `{}` is private", method_name, owner),
                        )
                        .with_label(Label::primary(span, "private method"))
                        .with_label(Label::secondary(mi.declaration_span, "method defined here"))
                        .with_help(format!("make it public with `pub fn {}`", method_name)),
                    );
                }
                if mi.params.len() != args.len() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "method `{}` takes {} argument(s) but {} were supplied",
                                method_name,
                                mi.params.len(),
                                args.len()
                            ),
                        )
                        .with_label(Label::primary(span, "wrong number of arguments")),
                    );
                }
                self.check_call_args_against_param_infos(&mi.param_infos, args);
                mi.return_type.clone()
            }
        }
    }

    fn resolve_static_call(
        &mut self,
        class_name: &str,
        method_name: &str,
        args: &[CallArg],
        span: Span,
    ) -> Type {
        if class_name == "f64" && method_name == "to_string" {
            if args.len() != 1 {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "function `f64::to_string` expects 1 argument, got {}",
                            args.len()
                        ),
                    )
                    .with_label(Label::primary(span, "wrong number of arguments")),
                );
            }
            let params = [Type::F64];
            self.check_value_call_args(&params, args);
            return Type::String;
        }

        // Check if `class_name` refers to an imported module (e.g. `math::add`).
        if let Some(module) = self.symbols.lookup_module(class_name).cloned() {
            return match module.functions.get(method_name).cloned() {
                None => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0350,
                            format!(
                                "function `{}` not found in module `{}`",
                                method_name, class_name
                            ),
                        )
                        .with_label(Label::primary(span, "not found in module")),
                    );
                    Type::Void
                }
                Some(fi) => {
                    if !fi.public {
                        let defined_at = fi
                            .module_path
                            .as_deref()
                            .map(|path| {
                                format!(
                                    "`{}` is defined at {}:{}:{}",
                                    method_name,
                                    path,
                                    fi.declaration_span.line,
                                    fi.declaration_span.col
                                )
                            })
                            .unwrap_or_else(|| {
                                format!(
                                    "`{}` is defined at line {}, column {}",
                                    method_name, fi.declaration_span.line, fi.declaration_span.col
                                )
                            });
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0402,
                                format!("function `{}` is private", method_name),
                            )
                            .with_label(Label::primary(span, "private function"))
                            .with_note(defined_at)
                            .with_help(format!("make it public with `pub fn {}`", method_name)),
                        );
                    }
                    if args.len() != fi.params.len() {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0203,
                                format!(
                                    "function `{}::{}` expects {} argument(s), got {}",
                                    class_name,
                                    method_name,
                                    fi.params.len(),
                                    args.len()
                                ),
                            )
                            .with_label(Label::primary(span, "wrong number of arguments")),
                        );
                    }
                    self.check_call_args_against_param_infos(&fi.param_infos, args);
                    fi.return_type.clone()
                }
            };
        }

        let class = match self.symbols.lookup_class(class_name).cloned() {
            Some(c) => c,
            None => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0350,
                        format!("unknown name `{}` (not a module or class)", class_name),
                    )
                    .with_label(Label::primary(span, "unknown module or class")),
                );
                return Type::Void;
            }
        };
        match class.methods.get(method_name).cloned() {
            None => {
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0502,
                    format!("no method `{}` on class `{}`", method_name, class_name),
                )
                .with_label(Label::primary(span, "method not found"));

                if let Some(suggestion) = suggest_similar_name(method_name, class.methods.keys()) {
                    diagnostic = diagnostic
                        .with_help(format!(
                            "there is a method with a similar name: `{}`",
                            suggestion
                        ))
                        .with_fix(FixSuggestion::new(
                            span,
                            suggestion,
                            "replace with suggested method",
                        ));
                }

                self.push(diagnostic);
                Type::Void
            }
            Some(mi) => {
                if !mi.public {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0501,
                            format!(
                                "method `{}` of class `{}` is private",
                                method_name, class_name
                            ),
                        )
                        .with_label(Label::primary(span, "private method"))
                        .with_label(Label::secondary(mi.declaration_span, "method defined here"))
                        .with_help(format!("make it public with `pub fn {}`", method_name)),
                    );
                }
                self.check_call_args_against_param_infos(&mi.param_infos, args);
                mi.return_type.clone()
            }
        }
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
        let mut current = Some(class_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return None;
            }
            let class = self.symbols.lookup_class(&name)?;
            if let Some(method) = class.methods.get(method_name) {
                return Some((name, method.clone()));
            }
            current = class.base_class.clone();
        }
        None
    }

    fn lookup_method_in_ancestors(
        &self,
        base_class_name: &str,
        method_name: &str,
    ) -> Option<(String, MethodInfo)> {
        self.lookup_method_in_hierarchy(base_class_name, method_name)
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

    fn check_block_with_narrowing(&mut self, block: &Block, narrowing: &NilCheckNarrowing) {
        self.narrowed_vars.push(HashMap::new());
        self.add_narrowing_to_current_scope(narrowing);
        self.check_block(block);
        self.narrowed_vars.pop();
    }

    fn add_narrowing_to_current_scope(&mut self, narrowing: &NilCheckNarrowing) {
        if let Some(scope) = self.narrowed_vars.last_mut() {
            scope.insert(
                narrowing.name.clone(),
                NarrowedVar {
                    ty: narrowing.narrowed_ty.clone(),
                    declaration_span: narrowing.declaration_span,
                },
            );
        }
    }

    fn clear_narrowing(&mut self, name: &str) {
        let Some(declaration_span) = self
            .symbols
            .lookup_var(name)
            .map(|info| info.declaration_span)
        else {
            return;
        };

        for scope in &mut self.narrowed_vars {
            if matches!(scope.get(name), Some(n) if n.declaration_span == declaration_span) {
                scope.remove(name);
            }
        }
    }

    fn lookup_narrowed_type(&self, name: &str) -> Option<Type> {
        let declaration_span = self.symbols.lookup_var(name)?.declaration_span;
        for scope in self.narrowed_vars.iter().rev() {
            if let Some(narrowed) = scope.get(name) {
                if narrowed.declaration_span == declaration_span {
                    return Some(narrowed.ty.clone());
                }
            }
        }
        None
    }

    fn nil_check_narrowing(&self, expr: &Expr) -> Option<NilCheckNarrowing> {
        let Expr::Binary(binary) = expr else {
            return None;
        };
        let non_nil_when_true = match binary.op {
            BinOp::Eq => false,
            BinOp::Ne => true,
            _ => return None,
        };
        let name = self.var_name_compared_with_nil(&binary.lhs, &binary.rhs)?;
        let info = self.symbols.lookup_var(name)?;
        let Type::Nullable(inner) = &info.ty else {
            return None;
        };
        Some(NilCheckNarrowing {
            name: name.to_string(),
            narrowed_ty: inner.as_ref().clone(),
            declaration_span: info.declaration_span,
            non_nil_when_true,
        })
    }

    fn var_name_compared_with_nil<'a>(&self, lhs: &'a Expr, rhs: &'a Expr) -> Option<&'a str> {
        match (lhs, rhs) {
            (Expr::Var(name, _), Expr::Nil(_)) | (Expr::Nil(_), Expr::Var(name, _)) => {
                Some(name.as_str())
            }
            _ => None,
        }
    }

    fn unify_ternary_types(&self, then_ty: &Type, else_ty: &Type) -> Option<Type> {
        if then_ty == else_ty {
            return Some(then_ty.clone());
        }

        match (then_ty, else_ty) {
            (Type::Nil, Type::Nil) => None,
            (Type::Nullable(_), Type::Nil) => Some(then_ty.clone()),
            (Type::Nil, Type::Nullable(_)) => Some(else_ty.clone()),
            (Type::Nil, other) => Some(Type::Nullable(Box::new(other.clone()))),
            (other, Type::Nil) => Some(Type::Nullable(Box::new(other.clone()))),
            (Type::Nullable(inner), other) if self.types_compatible(inner, other) => {
                Some(then_ty.clone())
            }
            (other, Type::Nullable(inner)) if self.types_compatible(inner, other) => {
                Some(else_ty.clone())
            }
            _ if self.types_compatible(then_ty, else_ty) => Some(then_ty.clone()),
            _ if self.types_compatible(else_ty, then_ty) => Some(else_ty.clone()),
            _ => None,
        }
    }

    fn check_nil_comparison(&mut self, lty: &Type, rty: &Type, span: Span) {
        match (lty, rty) {
            (Type::Nullable(_), Type::Nil) | (Type::Nil, Type::Nullable(_)) => {}
            (Type::Nil, Type::Nil) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        "cannot compare `nil` with `nil` without a nullable type context",
                    )
                    .with_label(Label::primary(span, "both sides are `nil`"))
                    .with_help("compare a nullable value with `nil` instead"),
                );
            }
            (Type::Nil, other) | (other, Type::Nil) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "cannot compare non-nullable type `{}` with `nil`",
                            type_name(other)
                        ),
                    )
                    .with_label(Label::primary(
                        span,
                        "only nullable values can be compared with `nil`",
                    ))
                    .with_help("make the value nullable with `?` or remove the `nil` comparison"),
                );
            }
            _ => {}
        }
    }

    fn validate_type(&mut self, ty: &Type, span: Span) {
        match ty {
            Type::Nullable(inner) => {
                if !nullable_inner_has_pointer_representation(inner) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "nullable primitive types are not implemented yet",
                        )
                        .with_label(Label::primary(
                            span,
                            format!("cannot lower `{}` yet", type_name(ty)),
                        ))
                        .with_help("use a wrapper class or avoid nullable primitive types for now"),
                    );
                }
                self.validate_type(inner, span);
            }
            Type::Array(element) => self.validate_type(element, span),
            Type::Generic(_, args) => {
                for arg in args {
                    self.validate_type(arg, span);
                }
            }
            Type::Fn(params, ret) => {
                for param in params {
                    self.validate_type(param, span);
                }
                self.validate_type(ret, span);
            }
            Type::I64
            | Type::F64
            | Type::Bool
            | Type::String
            | Type::Void
            | Type::Nil
            | Type::Named(_) => {}
        }
    }

    fn types_compatible(&self, expected: &Type, actual: &Type) -> bool {
        expected == actual
            || matches!(
                (expected, actual),
                (Type::Nullable(_), Type::Nil) | (Type::Nil, Type::Nullable(_))
            )
            || self.is_subtype(actual, expected)
    }

    fn is_subtype(&self, actual: &Type, expected: &Type) -> bool {
        match (actual, expected) {
            (Type::Named(child), Type::Named(parent)) => self.class_extends(child, parent),
            (Type::Nullable(actual_inner), Type::Nullable(expected_inner)) => {
                self.is_subtype(actual_inner, expected_inner)
            }
            (Type::Named(child), Type::Nullable(expected_inner)) => {
                self.is_subtype(&Type::Named(child.clone()), expected_inner)
            }
            _ => false,
        }
    }

    fn class_extends(&self, child: &str, parent: &str) -> bool {
        if child == parent {
            return true;
        }
        let mut current = Some(child.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return false;
            }
            let Some(class) = self.symbols.lookup_class(&name) else {
                return false;
            };
            let Some(base) = &class.base_class else {
                return false;
            };
            if base == parent {
                return true;
            }
            current = Some(base.clone());
        }
        false
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
            Type::Nullable(inner) => self.is_class_type(inner),
            _ => false,
        }
    }

    fn can_access_private_member(&self, owner: &str) -> bool {
        self.current_class.as_deref() == Some(owner)
    }

    fn push(&mut self, d: Diagnostic) {
        self.errors.push(d);
    }
}

fn type_name(ty: &Type) -> String {
    match ty {
        Type::I64 => "i64".to_string(),
        Type::F64 => "f64".to_string(),
        Type::Bool => "bool".to_string(),
        Type::String => "String".to_string(),
        Type::Void => "void".to_string(),
        Type::Nil => "nil".to_string(),
        Type::Named(n) => n.clone(),
        Type::Array(element) => format!("Array<{}>", type_name(element)),
        Type::Generic(name, args) => {
            let args = args.iter().map(type_name).collect::<Vec<_>>().join(", ");
            format!("{name}<{args}>")
        }
        Type::Nullable(inner) => format!("{}?", type_name(inner)),
        Type::Fn(params, ret) => {
            let param_str = params.iter().map(type_name).collect::<Vec<_>>().join(", ");
            format!("fn({}) -> {}", param_str, type_name(ret))
        }
    }
}

fn function_call_return_type(info: &FuncInfo) -> Type {
    if info.is_async {
        Type::Generic("Future".to_string(), vec![info.return_type.clone()])
    } else {
        info.return_type.clone()
    }
}

fn channel_element_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args) if name == "Channel" && args.len() == 1 => Some(args[0].clone()),
        _ => None,
    }
}

fn block_always_returns(block: &Block) -> bool {
    block.stmts.iter().any(stmt_always_returns)
}

fn stmt_always_returns(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Return(_) => true,
        Stmt::If(s) => s
            .else_block
            .as_ref()
            .map(|else_block| {
                block_always_returns(&s.then_block) && block_always_returns(else_block)
            })
            .unwrap_or(false),
        Stmt::Let(_) | Stmt::Assign(_) | Stmt::While(_) | Stmt::Expr(_) => false,
    }
}

fn nullable_inner_has_pointer_representation(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Named(_) | Type::String | Type::Array(_) | Type::Generic(_, _) | Type::Fn(_, _)
    )
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

fn class_info_from_decl(
    class: &ClassDecl,
    registered_name: &str,
    module_prefix: Option<&str>,
) -> ClassInfo {
    let mut fields = HashMap::new();
    let mut methods = HashMap::new();

    for field in &class.fields {
        fields.insert(
            field.name.clone(),
            FieldInfo {
                ty: qualify_type_for_module(&field.ty, module_prefix),
                public: field.public,
                declaration_span: field.span,
            },
        );
    }
    for method in &class.methods {
        let params = method
            .params
            .iter()
            .map(|param| qualify_type_for_module(&param.ty, module_prefix))
            .collect();
        methods.insert(
            method.name.clone(),
            MethodInfo {
                param_infos: param_infos_from_decl(&method.params, module_prefix),
                params,
                has_self: method.has_self,
                return_type: qualify_type_for_module(&method.return_type, module_prefix),
                public: method.public,
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
        base_class: class
            .base_class
            .as_ref()
            .map(|base| qualified_type_path_name(base, module_prefix)),
        declaration_span: class.span,
        fields,
        methods,
    }
}

fn qualify_type_for_module(ty: &Type, module_prefix: Option<&str>) -> Type {
    match ty {
        Type::Named(name) => module_prefix
            .filter(|_| !name.contains("::"))
            .map(|module| Type::Named(format!("{module}::{name}")))
            .unwrap_or_else(|| ty.clone()),
        Type::Array(element) => {
            Type::Array(Box::new(qualify_type_for_module(element, module_prefix)))
        }
        Type::Generic(name, args) => Type::Generic(
            module_prefix
                .filter(|_| !name.contains("::"))
                .map(|module| format!("{module}::{name}"))
                .unwrap_or_else(|| name.clone()),
            args.iter()
                .map(|arg| qualify_type_for_module(arg, module_prefix))
                .collect(),
        ),
        Type::Nullable(inner) => {
            Type::Nullable(Box::new(qualify_type_for_module(inner, module_prefix)))
        }
        Type::Fn(params, ret) => Type::Fn(
            params
                .iter()
                .map(|param| qualify_type_for_module(param, module_prefix))
                .collect(),
            Box::new(qualify_type_for_module(ret, module_prefix)),
        ),
        Type::I64 | Type::F64 | Type::Bool | Type::String | Type::Void | Type::Nil => ty.clone(),
    }
}

fn type_path_name(path: &TypePath) -> String {
    qualified_type_path_name(path, None)
}

fn qualified_type_path_name(path: &TypePath, module_prefix: Option<&str>) -> String {
    match path {
        TypePath::Local(name) => module_prefix
            .map(|module| format!("{module}::{name}"))
            .unwrap_or_else(|| name.clone()),
        TypePath::Qualified(parts) => parts.join("::"),
    }
}

fn is_supported_f64_format(spec: &str) -> bool {
    matches!(spec, "{:.17g}" | "{:.16f}" | "{:.6f}")
}

fn suggest_similar_name<'a>(
    target: &str,
    candidates: impl Iterator<Item = &'a String>,
) -> Option<String> {
    let max_distance = if target.len() <= 4 { 1 } else { 2 };
    candidates
        .map(|candidate| (levenshtein(target, candidate), candidate))
        .filter(|(distance, _)| *distance <= max_distance)
        .min_by_key(|(distance, candidate)| (*distance, candidate.len()))
        .map(|(_, candidate)| candidate.clone())
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b_chars = b.chars().collect::<Vec<_>>();
    let mut prev = (0..=b_chars.len()).collect::<Vec<_>>();
    let mut curr = vec![0; b_chars.len() + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn check_source(source: &str) -> Vec<Diagnostic> {
        let tokens = Lexer::new(source).tokenize().expect("lexing failed");
        let (program, parse_errors) = Parser::new(tokens).parse();
        assert!(
            parse_errors.is_empty(),
            "unexpected parse errors: {parse_errors:?}"
        );

        let mut checker = TypeChecker::new();
        checker.check_program(&program);
        checker.errors
    }

    fn assert_typecheck_ok(source: &str) {
        let errors = check_source(source);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    fn assert_typecheck_error_contains(source: &str, code: ErrorCode, expected_message: &str) {
        let errors = check_source(source);
        assert!(
            errors
                .iter()
                .any(|error| error.code == code && error.message.contains(expected_message)),
            "expected {code:?} containing `{expected_message}`, got {errors:?}",
        );
    }

    const NODE_CLASS: &str = r#"
class Node {
    pub value: i64;
    pub next: Node?;

    pub fn get(self) -> i64 {
        return self.value;
    }
}
"#;

    #[test]
    fn unit_inout_01_accepts_mutable_local_reference_argument() {
        assert_typecheck_ok(
            r#"
fn increment(x: inout i64) {
    x = x + 1;
}

fn f() {
    let mut n = 10;
    increment(&n);
}
"#,
        );
    }

    #[test]
    fn unit_inout_02_rejects_immutable_local_reference_argument() {
        assert_typecheck_error_contains(
            r#"
fn increment(x: inout i64) {
}

fn f() {
    let n = 10;
    increment(&n);
}
"#,
            ErrorCode::E1701,
            "cannot pass immutable variable `n` as `inout`",
        );
    }

    #[test]
    fn unit_inout_03_rejects_missing_reference_marker() {
        assert_typecheck_error_contains(
            r#"
fn increment(x: inout i64) {
}

fn f() {
    let mut n = 10;
    increment(n);
}
"#,
            ErrorCode::E1702,
            "expected reference argument for `inout` parameter",
        );
    }

    #[test]
    fn unit_inout_04_rejects_unexpected_reference_marker_for_value_param() {
        assert_typecheck_error_contains(
            r#"
fn take_value(x: i64) {
}

fn f() {
    let mut n = 10;
    take_value(&n);
}
"#,
            ErrorCode::E1703,
            "unexpected reference argument",
        );
    }

    #[test]
    fn unit_inout_05_rejects_non_place_reference_argument() {
        assert_typecheck_error_contains(
            r#"
fn increment(x: inout i64) {
}

fn f() {
    let mut n = 10;
    increment(&(n + 1));
}
"#,
            ErrorCode::E1704,
            "cannot pass non-place expression as `inout`",
        );
    }

    #[test]
    fn unit_inout_06_rejects_inout_argument_type_mismatch() {
        assert_typecheck_error_contains(
            r#"
fn set_bool(x: inout bool) {
}

fn f() {
    let mut n: i64 = 0;
    set_bool(&n);
}
"#,
            ErrorCode::E1705,
            "`inout` argument type mismatch",
        );
    }

    #[test]
    fn unit_nil_01_accepts_annotated_nullable_contexts() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn empty() -> Node? {{
    let node: Node? = nil;
    return nil;
}}
"#
        ));
    }

    #[test]
    fn unit_nil_02_rejects_unannotated_nil_local() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let value = nil;
}
"#,
            ErrorCode::E0201,
            "cannot infer the type of `nil`",
        );
    }

    #[test]
    fn unit_nil_03_rejects_nil_for_non_nullable_local() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let value: i64 = nil;
}
"#,
            ErrorCode::E0201,
            "mismatched types: expected `i64`, found `nil`",
        );
    }

    #[test]
    fn unit_nil_04_rejects_nil_for_non_nullable_return() {
        assert_typecheck_error_contains(
            &format!(
                r#"
{NODE_CLASS}

fn missing() -> Node {{
    return nil;
}}
"#
            ),
            ErrorCode::E0201,
            "mismatched types: expected `Node`, found `nil`",
        );
    }

    #[test]
    fn unit_nil_05_nullable_parameter_accepts_value_and_nil() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn visit(node: Node?) {{
}}

fn f(node: Node) {{
    visit(node);
    visit(nil);
}}
"#
        ));
    }

    #[test]
    fn unit_nil_06_rejects_nullable_value_for_non_nullable_parameter() {
        assert_typecheck_error_contains(
            &format!(
                r#"
{NODE_CLASS}

fn use_node(node: Node) {{
}}

fn f(node: Node?) {{
    use_node(node);
}}
"#
            ),
            ErrorCode::E0704,
            "mismatched types: expected `Node`, found `Node?`",
        );
    }

    #[test]
    fn unit_nil_07_object_literal_nullable_field_accepts_nil_and_value() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn make() -> Node {{
    let tail = Node {{ value: 2, next: nil }};
    return Node {{ value: 1, next: tail }};
}}
"#
        ));
    }

    #[test]
    fn unit_nil_08_rejects_direct_field_access_on_nullable_value() {
        assert_typecheck_error_contains(
            &format!(
                r#"
{NODE_CLASS}

fn value(node: Node?) -> i64 {{
    return node.value;
}}
"#
            ),
            ErrorCode::E0201,
            "cannot access field `value` on nullable type `Node?`",
        );
    }

    #[test]
    fn unit_nil_09_rejects_direct_method_call_on_nullable_value() {
        assert_typecheck_error_contains(
            &format!(
                r#"
{NODE_CLASS}

fn value(node: Node?) -> i64 {{
    return node.get();
}}
"#
            ),
            ErrorCode::E0201,
            "cannot call method `get` on nullable type `Node?`",
        );
    }

    #[test]
    fn unit_nil_10_if_not_nil_narrows_then_branch() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn value(node: Node?) -> i64 {{
    if node != nil {{
        return node.value;
    }}
    return 0;
}}
"#
        ));
    }

    #[test]
    fn unit_nil_11_nil_guard_return_narrows_following_code() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn value(node: Node?) -> i64 {{
    if node == nil {{
        return 0;
    }}
    return node.value;
}}
"#
        ));
    }

    #[test]
    fn unit_nil_12_nil_check_narrows_else_branch() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn value(node: Node?) -> i64 {{
    if node == nil {{
        return 0;
    }} else {{
        return node.value;
    }}
}}
"#
        ));
    }

    #[test]
    fn unit_nil_13_assignment_invalidates_narrowing() {
        assert_typecheck_error_contains(
            &format!(
                r#"
{NODE_CLASS}

fn value(node: Node?) -> i64 {{
    let mut current: Node? = node;
    if current != nil {{
        current = nil;
        return current.value;
    }}
    return 0;
}}
"#
            ),
            ErrorCode::E0201,
            "cannot access field `value` on nullable type `Node?`",
        );
    }

    #[test]
    fn unit_nil_14_ternary_unifies_value_and_nil_as_nullable() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn selected_is_missing(cond: bool, node: Node) -> bool {{
    let selected = cond ? node : nil;
    return selected == nil;
}}
"#
        ));
    }

    #[test]
    fn unit_nil_15_rejects_nil_comparison_with_non_nullable_value() {
        assert_typecheck_error_contains(
            r#"
fn f(value: i64) -> bool {
    return value == nil;
}
"#,
            ErrorCode::E0201,
            "cannot compare non-nullable type `i64` with `nil`",
        );
    }
}
