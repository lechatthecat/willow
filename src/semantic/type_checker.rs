use super::symbols::{
    ClassInfo, FieldInfo, FuncInfo, MethodInfo, ModuleInfo, SymbolTable, VarInfo,
};
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;

pub struct TypeChecker {
    pub symbols: SymbolTable,
    pub errors: Vec<Diagnostic>,
    current_return_type: Type,
    /// Stack of lambda return types being inferred. When non-empty, `return` stmts
    /// record their type here instead of checking against `current_return_type`.
    lambda_return_stack: Vec<Option<Type>>,
}

impl TypeChecker {
    pub fn new() -> Self {
        Self {
            symbols: SymbolTable::default(),
            errors: Vec::new(),
            current_return_type: Type::Void,
            lambda_return_stack: Vec::new(),
        }
    }

    /// Register an imported module's public items so cross-module calls can be resolved.
    pub fn register_module(&mut self, name: &str, program: &Program) {
        let mut functions = std::collections::HashMap::new();
        for item in &program.items {
            if let Item::Function(f) = item {
                if f.public {
                    functions.insert(
                        f.name.clone(),
                        FuncInfo {
                            params: f.params.iter().map(|p| p.ty.clone()).collect(),
                            return_type: f.return_type.clone(),
                            public: true,
                        },
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
                        params,
                        return_type: f.return_type.clone(),
                        public: f.public,
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
        let mut fields = std::collections::HashMap::new();
        let mut methods = std::collections::HashMap::new();

        for f in &c.fields {
            fields.insert(
                f.name.clone(),
                FieldInfo {
                    ty: f.ty.clone(),
                    public: f.public,
                },
            );
        }
        for m in &c.methods {
            let params = m.params.iter().map(|p| p.ty.clone()).collect();
            methods.insert(
                m.name.clone(),
                MethodInfo {
                    params,
                    has_self: m.has_self,
                    return_type: m.return_type.clone(),
                    public: m.public,
                    is_open: m.is_open,
                    is_override: m.is_override,
                },
            );
        }

        self.symbols.define_class(
            c.name.clone(),
            ClassInfo {
                name: c.name.clone(),
                public: c.public,
                is_open: c.is_open,
                fields,
                methods,
            },
        );
    }

    fn check_class(&mut self, c: &ClassDecl) {
        for m in &c.methods {
            self.check_method(m, &c.name);
        }
    }

    fn check_method(&mut self, m: &MethodDecl, class_name: &str) {
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
                    mutable: false,
                    is_param: true,
                    declaration_span: param.span,
                },
            );
        }

        self.check_block(&m.body);
        self.symbols.pop_scope();
    }

    fn check_function(&mut self, f: &FunctionDecl) {
        self.current_return_type = f.return_type.clone();
        self.symbols.push_scope();
        for param in &f.params {
            self.symbols.define_var(
                param.name.clone(),
                VarInfo {
                    ty: param.ty.clone(),
                    mutable: false,
                    is_param: true,
                    declaration_span: param.span,
                },
            );
        }
        self.check_block(&f.body);
        self.symbols.pop_scope();
    }

    fn check_block(&mut self, block: &Block) {
        self.symbols.push_scope();
        for stmt in &block.stmts {
            self.check_stmt(stmt);
        }
        self.symbols.pop_scope();
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => {
                let inferred = self.check_expr(&s.init);
                let ty = if let Some(ann) = &s.ty {
                    if !types_compatible(ann, &inferred) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "mismatched types: expected `{}`, found `{}`",
                                    type_name(ann),
                                    type_name(&inferred)
                                ),
                            )
                            .with_label(Label::primary(
                                s.span,
                                format!("expected `{}`", type_name(ann)),
                            )),
                        );
                    }
                    ann.clone()
                } else {
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
                                    )),
                                );
                            }
                        }
                        let got = self.check_expr(&s.value);
                        if !types_compatible(&info.ty, &got) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
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
                    }
                }
            }
            Stmt::If(s) => {
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
                self.check_block(&s.then_block);
                if let Some(else_b) = &s.else_block {
                    self.check_block(else_b);
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
                        )),
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
                if !types_compatible(&self.current_return_type, &ret_ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
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
            Expr::Var(name, span) => {
                // Local variable?
                if let Some(info) = self.symbols.lookup_var(name) {
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
                    for (param_ty, arg) in info.params.iter().zip(&c.args) {
                        let arg_ty = self.check_expr(arg);
                        if !types_compatible(param_ty, &arg_ty) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "mismatched types: expected `{}`, found `{}`",
                                        type_name(param_ty),
                                        type_name(&arg_ty)
                                    ),
                                )
                                .with_label(Label::primary(
                                    arg.span(),
                                    format!("expected `{}`", type_name(param_ty)),
                                )),
                            );
                        }
                    }
                    return info.return_type.clone();
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
                        for (param_ty, arg) in param_types.iter().zip(&c.args) {
                            let arg_ty = self.check_expr(arg);
                            if !types_compatible(param_ty, &arg_ty) {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "mismatched types: expected `{}`, found `{}`",
                                            type_name(param_ty),
                                            type_name(&arg_ty)
                                        ),
                                    )
                                    .with_label(Label::primary(
                                        arg.span(),
                                        format!("expected `{}`", type_name(param_ty)),
                                    )),
                                );
                            }
                        }
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
                let ret = self.resolve_method(&obj_ty, &m.method, &m.args, m.span);
                ret
            }
            Expr::StaticCall(s) => self.resolve_static_call(&s.class, &s.method, &s.args, s.span),
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
                if !types_compatible(&then_ty, &else_ty) {
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
                            format!("expected `{}`, found `{}`", type_name(&then_ty), type_name(&else_ty)),
                        ))
                        .with_label(Label::secondary(
                            t.then_expr.span(),
                            format!("this branch has type `{}`", type_name(&then_ty)),
                        )),
                    );
                    return Type::Void;
                }
                then_ty
            }
            Expr::Lambda(l) => self.check_lambda(l),
        }
    }

    fn check_lambda(&mut self, l: &LambdaExpr) -> Type {
        // All params must have type annotations (or infer from expected type — not yet supported).
        let mut param_types = Vec::new();
        for p in &l.params {
            match &p.ty {
                Some(ty) => param_types.push(ty.clone()),
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
                    let inferred = self.lambda_return_stack.pop().flatten().unwrap_or(Type::Void);
                    inferred
                }
            }
        };
        self.current_return_type = saved_ret_ty;
        self.symbols.pop_scope();

        let ret_ty = match &l.return_type {
            Some(ann) => {
                if !types_compatible(ann, &body_ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
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

    fn check_binary(&mut self, b: &BinaryExpr) -> Type {
        let lty = self.check_expr(&b.lhs);
        let rty = self.check_expr(&b.rhs);

        match &b.op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
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
                if !types_compatible(&lty, &rty) {
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
        let class = match self.symbols.lookup_class(&class_name).cloned() {
            Some(c) => c,
            None => {
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
        };
        match class.fields.get(field_name) {
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
            Some(fi) => {
                if check_visibility && !fi.public {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0501,
                            format!(
                                "field `{}` of class `{}` is private",
                                field_name, class_name
                            ),
                        )
                        .with_label(Label::primary(span, "private field"))
                        .with_help(format!("use `pub {}` to make it public", field_name)),
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
        args: &[Expr],
        span: Span,
    ) -> Type {
        let class_name = match obj_ty {
            Type::Named(n) => n.clone(),
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
        let class = match self.symbols.lookup_class(&class_name).cloned() {
            Some(c) => c,
            None => {
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
        };
        match class.methods.get(method_name).cloned() {
            None => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0502,
                        format!("no method `{}` on class `{}`", method_name, class_name),
                    )
                    .with_label(Label::primary(span, "method not found")),
                );
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
                        .with_label(Label::primary(span, "private method")),
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
                for (param_ty, arg) in mi.params.iter().zip(args) {
                    let arg_ty = self.check_expr(arg);
                    if !types_compatible(param_ty, &arg_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "mismatched types: expected `{}`, found `{}`",
                                    type_name(param_ty),
                                    type_name(&arg_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                arg.span(),
                                format!("expected `{}`", type_name(param_ty)),
                            )),
                        );
                    }
                }
                mi.return_type.clone()
            }
        }
    }

    fn resolve_static_call(
        &mut self,
        class_name: &str,
        method_name: &str,
        args: &[Expr],
        span: Span,
    ) -> Type {
        // Check if `class_name` refers to an imported module (e.g. `math::add`).
        if let Some(module) = self.symbols.lookup_module(class_name).cloned() {
            return match module.functions.get(method_name).cloned() {
                None => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0402,
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
                    for (param_ty, arg) in fi.params.iter().zip(args) {
                        let arg_ty = self.check_expr(arg);
                        if !types_compatible(param_ty, &arg_ty) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "mismatched types: expected `{}`, found `{}`",
                                        type_name(param_ty),
                                        type_name(&arg_ty)
                                    ),
                                )
                                .with_label(Label::primary(
                                    arg.span(),
                                    format!("expected `{}`", type_name(param_ty)),
                                )),
                            );
                        }
                    }
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
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0502,
                        format!("no method `{}` on class `{}`", method_name, class_name),
                    )
                    .with_label(Label::primary(span, "method not found")),
                );
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
                        .with_label(Label::primary(span, "private method")),
                    );
                }
                for (param_ty, arg) in mi.params.iter().zip(args) {
                    let arg_ty = self.check_expr(arg);
                    if !types_compatible(param_ty, &arg_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "mismatched types: expected `{}`, found `{}`",
                                    type_name(param_ty),
                                    type_name(&arg_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                arg.span(),
                                format!("expected `{}`", type_name(param_ty)),
                            )),
                        );
                    }
                }
                mi.return_type.clone()
            }
        }
    }

    fn push(&mut self, d: Diagnostic) {
        self.errors.push(d);
    }
}

fn types_compatible(a: &Type, b: &Type) -> bool {
    a == b
}

fn type_name(ty: &Type) -> String {
    match ty {
        Type::I64 => "i64".to_string(),
        Type::F64 => "f64".to_string(),
        Type::Bool => "bool".to_string(),
        Type::Void => "void".to_string(),
        Type::Named(n) => n.clone(),
        Type::Fn(params, ret) => {
            let param_str = params.iter().map(type_name).collect::<Vec<_>>().join(", ");
            format!("fn({}) -> {}", param_str, type_name(ret))
        }
    }
}
