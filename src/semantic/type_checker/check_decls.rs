use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::semantic::symbols::*;
use std::collections::HashSet;

use super::*;

impl TypeChecker {
    /// Validate an interface's `extends` clause (willow-1js.2 / willow-1js.8):
    /// each super must be a registered interface, with no cycle.
    pub(super) fn check_interface(&mut self, decl: &InterfaceDecl) {
        // Each super-interface must exist and be an interface.
        for sup in &decl.extends {
            if self.symbols.lookup_interface(sup).is_none() {
                if self.symbols.lookup_class(sup).is_some() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0411,
                            format!("`{sup}` is a class, not an interface"),
                        )
                        .with_label(Label::primary(decl.span, "cannot extend a class"))
                        .with_help("interfaces may only `extends` other interfaces"),
                    );
                } else {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0410,
                            format!("cannot find interface `{sup}`"),
                        )
                        .with_label(Label::primary(decl.span, "unknown super-interface")),
                    );
                }
            }
        }
        // Detect an `extends` cycle (e.g. `A extends B`, `B extends A`).
        if self.interface_extends(&decl.name, &decl.name) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0423,
                    format!("cyclic interface inheritance involving `{}`", decl.name),
                )
                .with_label(Label::primary(
                    decl.span,
                    "interface cannot transitively extend itself",
                )),
            );
        }
        // Type-check default method bodies even when no class implements the
        // interface (willow-1js.7). `self` is the interface type; the body is the
        // canonical default (the injected class copies of non-generic defaults are
        // skipped in check_class to avoid duplicates). Generic interfaces are
        // skipped — their type parameters are not concrete, so the body is checked
        // through each implementing class's substituted copy instead.
        if decl.type_params.is_empty() {
            for m in &decl.methods {
                if m.default_body.is_some() {
                    self.check_interface_default_body(m, &decl.name);
                }
            }
        }
    }

    /// Type-check an interface default method body with `self` bound to the
    /// interface type (willow-1js.7). Mirrors `check_method`'s scope handling.
    pub(super) fn check_interface_default_body(
        &mut self,
        m: &InterfaceMethodDecl,
        iface_name: &str,
    ) {
        let Some(body) = &m.default_body else {
            return;
        };
        let return_type = self.normalize_type(&m.return_type, m.span);
        let param_types = self.normalize_param_types(&m.params);
        let previous_class = self.current_class.replace(iface_name.to_string());
        let previous_async = self.current_async_context;
        let previous_static = self.in_static_method;
        let previous_return = std::mem::replace(&mut self.current_return_type, return_type);
        self.current_async_context = false;
        self.in_static_method = false;
        self.symbols.push_scope();
        self.symbols.define_var(
            "self".to_string(),
            VarInfo {
                ty: Type::Named(iface_name.to_string()),
                mutable: false,
                is_param: true,
                declaration_span: m.span,
            },
        );
        for (param, ty) in m.params.iter().zip(param_types.iter()) {
            self.symbols.define_var(
                param.name.clone(),
                VarInfo {
                    ty: ty.clone(),
                    mutable: matches!(&param.mode, ParamMode::Reference { mutable: true, .. }),
                    is_param: true,
                    declaration_span: param.span,
                },
            );
        }
        self.check_block(body);
        self.symbols.pop_scope();
        self.current_class = previous_class;
        self.current_async_context = previous_async;
        self.in_static_method = previous_static;
        self.current_return_type = previous_return;
    }

    pub(super) fn check_collection_type_imported(&mut self, name: &str, span: Span) {
        if self.imported_collection_types.contains(name)
            || self.fully_qualified_collection_types.contains(name)
            || self
                .imported_collection_aliases
                .values()
                .any(|item| item == name)
        {
            return;
        }
        if !self
            .missing_collection_imports_reported
            .insert(name.to_string())
        {
            return;
        }
        let (code, help) = match name {
            "Array" => (ErrorCode::E2001, "add `import std::collections::Array;`"),
            "Map" => (ErrorCode::E2002, "add `import std::collections::Map;`"),
            _ => return,
        };
        self.push(
            Diagnostic::new(
                Severity::Error,
                code,
                format!("cannot find type `{name}` in scope"),
            )
            .with_label(Label::primary(span, "collection type requires an import"))
            .with_help(help),
        );
    }

    pub(super) fn check_class(&mut self, c: &ClassDecl) {
        self.check_class_inheritance(c);
        self.check_constructor_inheritance_rules(c);
        self.check_class_implements(c);
        for field in &c.fields {
            let ty = self.normalize_type(&field.ty, field.span);
            self.validate_type(&ty, field.span);
        }
        self.check_static_property_initializers(c);
        for m in &c.methods {
            // A non-generic interface default body is checked once at the
            // interface level; skip the injected class copy to avoid duplicate
            // diagnostics (willow-1js.7).
            if m.is_default_injected {
                continue;
            }
            self.check_method(m, &c.name);
        }
        for ctor in &c.constructors {
            self.check_constructor(ctor, c);
        }
    }

    /// Type-check an `init(...)` constructor body (willow-scq2): bind `self`,
    /// reject returning a value (E0841), and require every instance field to be
    /// assigned (E0842).
    pub(super) fn check_constructor(&mut self, ctor: &ConstructorDecl, c: &ClassDecl) {
        let param_types = self.normalize_param_types(&ctor.params);
        for (param, ty) in ctor.params.iter().zip(param_types.iter()) {
            self.validate_type(ty, param.span);
        }
        let previous_class = self.current_class.replace(c.name.clone());
        let previous_return = std::mem::replace(&mut self.current_return_type, Type::Void);
        let previous_ctor = self.in_constructor;
        self.in_constructor = true;
        self.symbols.push_scope();
        // `self` is the new instance being constructed.
        self.symbols.define_var(
            "self".to_string(),
            VarInfo {
                ty: Type::Named(c.name.clone()),
                mutable: false,
                is_param: true,
                declaration_span: ctor.span,
            },
        );
        for (param, ty) in ctor.params.iter().zip(param_types.iter()) {
            self.symbols.define_var(
                param.name.clone(),
                VarInfo {
                    ty: ty.clone(),
                    mutable: matches!(&param.mode, ParamMode::Reference { mutable: true, .. }),
                    is_param: true,
                    declaration_span: param.span,
                },
            );
        }
        // `return <value>` is reported as E0841 by the Stmt::Return check while
        // `in_constructor` is set.
        self.check_block(&ctor.body);

        // Every instance field must be assigned in the constructor body
        // (willow-scq2 §8 → E0842). MVP check: a `self.field = ...` exists for
        // each field somewhere in the body (not path-sensitive).
        let mut assigned: HashSet<String> = HashSet::new();
        collect_self_field_assigns(&ctor.body, &mut assigned);
        for field in &c.fields {
            if field.is_static {
                continue;
            }
            if !assigned.contains(&field.name) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0842,
                        format!(
                            "field `{}` is not initialized by constructor `{}::init`",
                            field.name, c.name
                        ),
                    )
                    .with_label(Label::primary(ctor.span, "field left uninitialized"))
                    .with_help(format!(
                        "assign `self.{} = ...` in the constructor",
                        field.name
                    )),
                );
            }
        }

        self.symbols.pop_scope();
        self.current_class = previous_class;
        self.current_return_type = previous_return;
        self.in_constructor = previous_ctor;
    }

    pub(super) fn check_constructor_inheritance_rules(&mut self, c: &ClassDecl) {
        if c.constructors.is_empty() {
            return;
        }
        let Some(base_name) = c.base_class.as_ref().map(type_path_name) else {
            return;
        };
        for ctor in &c.constructors {
            let mut super_init_spans = Vec::new();
            collect_super_init_spans(&ctor.body, &mut super_init_spans);
            if super_init_spans.len() > 1 {
                for span in super_init_spans.iter().skip(1) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0848,
                            "`super.init(...)` can only be called once",
                        )
                        .with_label(Label::primary(*span, "duplicate base initialization")),
                    );
                }
            }

            if let Some(span) = super_init_spans.first().copied() {
                if !matches!(ctor.body.stmts.first(), Some(Stmt::SuperInit(_))) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0848,
                            "`super.init(...)` must be the first statement in a constructor",
                        )
                        .with_label(Label::primary(span, "move this call to the top"))
                        .with_help("call `super.init(...)` before assigning fields or branching"),
                    );
                }
            } else if let Some(base) = self.base_class_requiring_initialization(&base_name) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0848,
                        format!(
                            "constructor `{}::init` must call `super.init(...)` to initialize base class `{}`",
                            c.name, base_name
                        ),
                    )
                    .with_label(Label::primary(
                        ctor.span,
                        "missing base constructor call",
                    ))
                    .with_label(Label::secondary(
                        base.declaration_span,
                        "base class requires initialization",
                    ))
                    .with_help("add `super.init(...)` as the first statement in this constructor"),
                );
            }
        }
    }

    pub(super) fn check_super_init(&mut self, s: &SuperInitStmt) {
        if !self.in_constructor {
            for arg in &s.args {
                self.check_expr(&arg.expr);
            }
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0848,
                    "`super.init(...)` can only be used inside a constructor",
                )
                .with_label(Label::primary(s.span, "not inside `init`")),
            );
            return;
        }

        let Some(current_class) = self.current_class.clone() else {
            for arg in &s.args {
                self.check_expr(&arg.expr);
            }
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0848,
                    "`super.init(...)` can only be used inside a class constructor",
                )
                .with_label(Label::primary(s.span, "no current class")),
            );
            return;
        };

        let Some(base_name) = self
            .symbols
            .lookup_class(&current_class)
            .and_then(|class| class.base_class.clone())
        else {
            for arg in &s.args {
                self.check_expr(&arg.expr);
            }
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0848,
                    format!("class `{current_class}` has no base class for `super.init(...)`"),
                )
                .with_label(Label::primary(s.span, "no base class")),
            );
            return;
        };

        let Some(base) = self.symbols.lookup_class(&base_name).cloned() else {
            for arg in &s.args {
                self.check_expr(&arg.expr);
            }
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0350,
                    format!("base class `{base_name}` not found"),
                )
                .with_label(Label::primary(s.span, "unknown base class")),
            );
            return;
        };

        let param_infos: Vec<ParamInfo> = match base.constructor.clone() {
            Some(ci) => {
                if !ci.public {
                    let allowed = if ci.protected {
                        self.can_access_protected_member(&base_name)
                    } else {
                        self.can_access_private_member(&base_name)
                    };
                    if !allowed {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0846,
                                format!("constructor of `{base_name}` is not visible here"),
                            )
                            .with_label(Label::primary(s.span, "constructor not visible"))
                            .with_label(Label::secondary(
                                ci.declaration_span,
                                "constructor declared here",
                            )),
                        );
                    }
                }
                ci.param_infos
            }
            None => {
                let fields = self.implicit_constructor_field_infos(&base_name);
                self.check_implicit_constructor_field_visibility(&fields, &s.args, s.span);
                let params = fields
                    .iter()
                    .map(|(_, _, field)| field.ty.clone())
                    .collect::<Vec<_>>();
                value_param_infos(&params)
            }
        };

        self.check_call_argument_count(
            "`super.init(...)`",
            param_infos.len(),
            s.args.len(),
            s.span,
        );
        self.check_call_args_against_param_infos(&param_infos, &s.args);
    }

    /// Type-check `new Class(args...)` (willow-scq2 §10): resolve the class, pick
    /// the explicit `init` or the implicit memberwise constructor, check
    /// visibility and arguments, and yield `Class`.
    pub(super) fn check_new(&mut self, n: &NewExpr) -> Type {
        // Evaluate args first so their own errors surface regardless of arity.
        let arg_types: Vec<Type> = n.args.iter().map(|a| self.check_expr(&a.expr)).collect();
        let resolved = self
            .resolve_static_call_class_name(&n.class_name, n.span)
            .unwrap_or_else(|| n.class_name.clone());

        let Some(class) = self.symbols.lookup_class(&resolved).cloned() else {
            if self.symbols.lookup_interface(&resolved).is_some() {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0413,
                        format!("cannot instantiate interface `{}`", resolved),
                    )
                    .with_label(Label::primary(n.span, "interfaces have no constructor"))
                    .with_help("instantiate a class that implements this interface instead"),
                );
            } else {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0844,
                        format!("unknown class `{}`", resolved),
                    )
                    .with_label(Label::primary(n.span, "no such class")),
                );
            }
            return Type::Void;
        };

        // Constructor signature: explicit `init`, else implicit memberwise
        // (inherited fields first, then this class's fields in declaration
        // order, matching the runtime object layout).
        let params: Vec<Type> = match &class.constructor {
            Some(ci) => {
                // Visibility of an explicit constructor (willow-scq2 §9).
                if !ci.public {
                    let allowed = if ci.protected {
                        self.can_access_protected_member(&resolved)
                    } else {
                        self.can_access_private_member(&resolved)
                    };
                    if !allowed {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0846,
                                format!("constructor of `{}` is not visible here", resolved),
                            )
                            .with_label(Label::primary(n.span, "private constructor"))
                            .with_help("construct it through a visible factory method"),
                        );
                    }
                }
                ci.params.clone()
            }
            None => {
                let fields = self.implicit_constructor_field_infos(&resolved);
                self.check_implicit_constructor_field_visibility(&fields, &n.args, n.span);
                fields
                    .iter()
                    .map(|(_, _, field)| field.ty.clone())
                    .collect()
            }
        };

        if arg_types.len() != params.len() {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0845,
                    format!(
                        "constructor `{}::init` expects {} argument(s) but got {}",
                        resolved,
                        params.len(),
                        arg_types.len()
                    ),
                )
                .with_label(Label::primary(n.span, "wrong number of arguments")),
            );
        } else {
            for (i, (aty, pty)) in arg_types.iter().zip(params.iter()).enumerate() {
                if !self.types_compatible(pty, aty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(pty, aty),
                            format!(
                                "constructor argument {} of `{}` expects `{}`, found `{}`",
                                i + 1,
                                resolved,
                                type_name(pty),
                                type_name(aty)
                            ),
                        )
                        .with_label(Label::primary(
                            n.args[i].expr.span(),
                            format!("expected `{}`", type_name(pty)),
                        )),
                    );
                }
            }
        }

        Type::Named(resolved)
    }

    pub(super) fn check_implicit_constructor_field_visibility(
        &mut self,
        fields: &[(String, String, FieldInfo)],
        args: &[CallArg],
        call_span: Span,
    ) {
        for (idx, (owner, field_name, field)) in fields.iter().enumerate() {
            if field.public {
                continue;
            }
            let span = args.get(idx).map(|arg| arg.span).unwrap_or(call_span);
            if field.protected {
                if !self.can_access_protected_member(owner) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0503,
                            format!("field `{}` of class `{}` is protected", field_name, owner),
                        )
                        .with_label(Label::primary(
                            span,
                            "memberwise constructor initializes a protected field",
                        ))
                        .with_label(Label::secondary(
                            field.declaration_span,
                            "field defined here",
                        ))
                        .with_help(format!(
                            "provide a visible `init` or factory method on `{}`",
                            owner
                        )),
                    );
                }
            } else if !self.can_access_private_member(owner) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0501,
                        format!("field `{}` of class `{}` is private", field_name, owner),
                    )
                    .with_label(Label::primary(
                        span,
                        "memberwise constructor initializes a private field",
                    ))
                    .with_label(Label::secondary(
                        field.declaration_span,
                        "field defined here",
                    ))
                    .with_help(format!(
                        "provide a public `init` or factory method on `{}`",
                        owner
                    )),
                );
            }
        }
    }

    /// Type-check `static [mut] name: T = expr` initializers (willow-qsqf §10).
    /// Each initializer must be assignable to the declared type, cannot use
    /// `self`, and may only reference earlier static properties of the same class
    /// (no forward references / cycles in MVP).
    pub(super) fn check_static_property_initializers(&mut self, c: &ClassDecl) {
        // Static properties declared so far in this class, in order — used to
        // reject forward references (`static b = C::a` before `a`).
        let mut initialized: HashSet<String> = HashSet::new();
        let previous_class = self.current_class.replace(c.name.clone());
        for field in &c.fields {
            if !field.is_static {
                continue;
            }
            let Some(init) = &field.initializer else {
                continue; // parser already requires an initializer for static
            };
            let declared = self.normalize_type(&field.ty, field.span);

            // Reject forward references to not-yet-initialized statics of THIS
            // class (`C::later` used before `later` is declared).
            self.check_static_forward_references(init, &c.name, &initialized);

            let previous_init_ctx = self.in_static_initializer;
            self.in_static_initializer = true;
            let init_ty = self.check_expr(init);
            self.in_static_initializer = previous_init_ctx;

            if !self.types_compatible(&declared, &init_ty) && init_ty != Type::Void {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0301,
                        format!(
                            "static property `{}::{}` has type `{}` but its initializer is `{}`",
                            c.name,
                            field.name,
                            type_name(&declared),
                            type_name(&init_ty)
                        ),
                    )
                    .with_label(Label::primary(init.span(), "initializer type mismatch")),
                );
            }
            initialized.insert(field.name.clone());
        }
        self.current_class = previous_class;
    }

    /// Walk an initializer expression and reject `C::prop` references to static
    /// properties of the same class `C` that are not yet initialized
    /// (willow-qsqf §10.4 → E0838).
    pub(super) fn check_static_forward_references(
        &mut self,
        expr: &Expr,
        class_name: &str,
        initialized: &HashSet<String>,
    ) {
        if let Expr::StaticField(s) = expr {
            // `Self::x` or `ClassName::x` referring to this class.
            let refers_self = s.class == "Self" || s.class == class_name;
            if refers_self
                && !initialized.contains(&s.field)
                && self
                    .symbols
                    .lookup_class(class_name)
                    .is_some_and(|c| c.static_props.contains_key(&s.field))
            {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0838,
                        format!(
                            "static property `{}::{}` is used before it is initialized",
                            class_name, s.field
                        ),
                    )
                    .with_label(Label::primary(s.span, "used before initialization"))
                    .with_help("declare it earlier, or reorder the static properties"),
                );
            }
        }
        walk_subexprs(expr, &mut |sub| {
            self.check_static_forward_references(sub, class_name, initialized)
        });
    }

    /// Validate a class's `implements` clause: each named interface must exist
    /// and be an interface, must not be repeated, and the class (including its
    /// inherited methods) must satisfy every required method signature exactly.
    pub(super) fn check_class_implements(&mut self, c: &ClassDecl) {
        let mut seen: HashSet<String> = HashSet::new();
        for iface_ty in &c.implements {
            // Split the implemented interface into its name and type arguments:
            // `Animal` -> ("Animal", []), `From<Err>` -> ("From", [Err]).
            let (iface_name, type_args): (String, Vec<Type>) = match iface_ty {
                Type::Named(n) => (n.clone(), Vec::new()),
                Type::Generic(n, args) => (n.clone(), args.clone()),
                other => (type_name(other), Vec::new()),
            };

            // `Send` / `Sync` are compiler-known marker interfaces: the compiler
            // infers them from a type's structure, so they may not be implemented
            // manually (willow-dgwo, E2401). Incorrect manual `Sync` could make
            // data races possible.
            if iface_name == "Send" || iface_name == "Sync" {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E2401,
                        format!(
                            "`{iface_name}` is a compiler-known marker interface and cannot be implemented manually"
                        ),
                    )
                    .with_label(Label::primary(
                        c.span,
                        format!("cannot manually implement `{iface_name}`"),
                    ))
                    .with_help(
                        "`Send`/`Sync` are inferred from a type's fields; for shared mutable state use `Mutex<T>`, `RwLock<T>`, `Atomic*`, `Channel<T>`, or frozen data",
                    ),
                );
                continue;
            }

            // A class may implement a given interface instantiation at most once,
            // keyed by the FULL instantiated type (name + type arguments). Two
            // distinct instantiations of the same generic interface
            // (`Container<i64>`, `Container<String>`) are allowed (willow-1js.6):
            // each compiled class method is monomorphic and a generic interface's
            // vtable slot order is independent of its type arguments, so all
            // instantiations of one interface on one class share a single,
            // byte-identical vtable (keyed by interface name in codegen — see
            // `declare_one_vtable`). Conformance still rejects any case where one
            // method body cannot satisfy every instantiation (e.g. `get(self)->T`
            // cannot return both `i64` and `String`, E0417); only interfaces whose
            // type parameters appear in no method signature can be implemented at
            // multiple instantiations. An EXACT-duplicate instantiation
            // (`Container<i64>` twice) remains an error.
            let inst_key = type_name(iface_ty);
            if !seen.insert(inst_key.clone()) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0414,
                        format!("interface `{inst_key}` is implemented more than once"),
                    )
                    .with_label(Label::primary(c.span, "duplicate interface"))
                    .with_help(
                        "a class may implement a given interface instantiation only once; remove the duplicate",
                    ),
                );
                continue;
            }

            // Resolve: interface? class (wrong kind)? or unknown?
            let iface = match self.symbols.lookup_interface(&iface_name) {
                Some(info) => info.clone(),
                None => {
                    if self.symbols.lookup_class(&iface_name).is_some() {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0411,
                                format!("`{iface_name}` is a class, not an interface"),
                            )
                            .with_label(Label::primary(c.span, "not an interface"))
                            .with_help("a class can only `implements` interfaces; use `extends` for a base class"),
                        );
                    } else {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0410,
                                format!("cannot find interface `{iface_name}`"),
                            )
                            .with_label(Label::primary(c.span, "unknown interface"))
                            .with_help(
                                "define an `interface` with this name, or check the spelling",
                            ),
                        );
                    }
                    continue;
                }
            };

            // Type-argument arity must match the interface's generic parameters.
            if type_args.len() != iface.type_params.len() {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0422,
                        format!(
                            "interface `{}` takes {} type argument(s), but {} were given",
                            iface_name,
                            iface.type_params.len(),
                            type_args.len()
                        ),
                    )
                    .with_label(Label::primary(c.span, "wrong number of type arguments")),
                );
                continue;
            }

            // Instantiate the interface for this class: substitute its type
            // parameters with the given arguments and `Self` with the class
            // (so `fn from(e: E) -> Self` conforms to a concrete signature).
            let instantiated =
                self.instantiate_interface(&iface, &type_args, &Type::Named(c.name.clone()));
            self.check_interface_conformance(c, &instantiated);
        }
    }

    /// Check that class `c` provides every method required by `iface` with an
    /// exact (MVP: invariant) signature match. Inherited methods count.
    pub(super) fn check_interface_conformance(&mut self, c: &ClassDecl, iface: &InterfaceInfo) {
        for req_name in &iface.method_order {
            let req = &iface.methods[req_name];
            // A method declared on the class itself or inherited from an ancestor
            // can satisfy the requirement.
            let found = self.lookup_method_in_hierarchy(&c.name, req_name);
            let Some((owner, method)) = found else {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0415,
                        format!(
                            "class `{}` does not implement interface `{}`",
                            c.name, iface.name
                        ),
                    )
                    .with_label(Label::primary(
                        c.span,
                        format!("missing method `{}`", interface_method_signature(req)),
                    ))
                    .with_label(Label::secondary(
                        req.declaration_span,
                        "required by this interface method",
                    ))
                    .with_help(format!("add `pub fn {}` to `{}`", req_name, c.name)),
                );
                continue;
            };

            // The implementing method must be public so it is callable through
            // the interface reference.
            if !method.public {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0415,
                        format!(
                            "method `{}` on `{}` must be `pub` to satisfy interface `{}`",
                            req_name, owner, iface.name
                        ),
                    )
                    .with_label(Label::primary(method.declaration_span, "method is private"))
                    .with_help("interface methods are public by contract; mark it `pub`"),
                );
            }

            // Receiver compatibility: an interface instance method requires an
            // instance method to satisfy it. With implicit `self`, the
            // implementing method need not write `self` explicitly — it just must
            // not be `static` (a static method has no receiver). (willow-qsqf)
            if req.has_self && method.is_static {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0416,
                        format!(
                            "static method `{}` on `{}` cannot satisfy instance method of interface `{}`",
                            req_name, owner, iface.name
                        ),
                    )
                    .with_label(Label::primary(
                        method.declaration_span,
                        "static method has no `self` receiver",
                    ))
                    .with_label(Label::secondary(
                        req.declaration_span,
                        "interface requires an instance method",
                    )),
                );
            }

            // Parameter count and types must match exactly (no variance in MVP).
            if method.params != req.params {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0416,
                        format!(
                            "method `{}` parameters do not match interface `{}`",
                            req_name, iface.name
                        ),
                    )
                    .with_label(Label::primary(
                        method.declaration_span,
                        format!(
                            "found `({})`",
                            method
                                .params
                                .iter()
                                .map(type_name)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ))
                    .with_label(Label::secondary(
                        req.declaration_span,
                        format!(
                            "interface requires `({})`",
                            req.params
                                .iter()
                                .map(type_name)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )),
                );
            }

            // Return type must match the callable surface exactly. For an
            // `async` method, that surface is `Task<T>` even though the method
            // declaration writes the awaited value `T`.
            let actual_return_type = method_call_return_type(&method);
            if actual_return_type != req.return_type {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0417,
                        format!(
                            "method `{}` returns `{}`, but interface `{}` requires `{}`",
                            req_name,
                            type_name(&actual_return_type),
                            iface.name,
                            type_name(&req.return_type)
                        ),
                    )
                    .with_label(Label::primary(
                        method.declaration_span,
                        "return type mismatch",
                    ))
                    .with_label(Label::secondary(
                        req.declaration_span,
                        "required return type declared here",
                    )),
                );
            }
        }
    }

    pub(super) fn check_class_inheritance(&mut self, c: &ClassDecl) {
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
                // A class may not extend an interface; that is what `implements` is for.
                if self.symbols.lookup_interface(&base_name).is_some() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0412,
                            format!("`{base_name}` is an interface and cannot be extended"),
                        )
                        .with_label(Label::primary(c.span, "cannot `extends` an interface"))
                        .with_help(format!("use `implements {base_name}` instead of `extends`")),
                    );
                    return;
                }
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

        // Static members are non-virtual: a subclass may not redefine (hide) a
        // static member inherited from a base class (willow-qsqf §16.3 → E0839).
        for method in &c.methods {
            if method.is_static {
                if let Some((owner, _)) = self.lookup_method_in_hierarchy(&base_name, &method.name)
                {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0839,
                            format!(
                                "static member `{}::{}` hides inherited static member `{}::{}`",
                                c.name, method.name, owner, method.name
                            ),
                        )
                        .with_label(Label::primary(
                            method.span,
                            "hides an inherited static member",
                        ))
                        .with_help("use a different name"),
                    );
                }
            }
        }
        for field in &c.fields {
            if field.is_static {
                if let Some((owner, _)) =
                    self.lookup_static_prop_in_hierarchy(&base_name, &field.name)
                {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0839,
                            format!(
                                "static member `{}::{}` hides inherited static member `{}::{}`",
                                c.name, field.name, owner, field.name
                            ),
                        )
                        .with_label(Label::primary(
                            field.span,
                            "hides an inherited static member",
                        ))
                        .with_help("use a different name"),
                    );
                }
            }
        }

        for method in &c.methods {
            // Static methods participate in the class namespace but are not
            // inherited/overridable like instance methods, so skip override
            // validation for them. Instance methods (implicit or explicit `self`)
            // are still validated (willow-qsqf).
            if method.is_static {
                continue;
            }
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
                        .map(|param| self.normalize_type(&param.ty, param.type_span))
                        .collect::<Vec<_>>();
                    let method_return_type = self.normalize_type(&method.return_type, method.span);
                    let actual_call_return_type = if method.is_async {
                        Type::Generic("Task".to_string(), vec![method_return_type.clone()])
                    } else {
                        method_return_type.clone()
                    };
                    let base_call_return_type = method_call_return_type(&base_method);
                    if method_params != base_method.params
                        || actual_call_return_type != base_call_return_type
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

    pub(super) fn check_method(&mut self, m: &MethodDecl, class_name: &str) {
        let return_type = self.normalize_type(&m.return_type, m.span);
        let param_types = self.normalize_param_types(&m.params);
        self.validate_type(&return_type, m.span);
        for (param, ty) in m.params.iter().zip(param_types.iter()) {
            self.validate_type(ty, param.span);
        }
        if m.is_async && is_task_handle_type(&return_type) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0809,
                    "async method return type must be the awaited value, not a task handle",
                )
                .with_label(Label::primary(
                    m.span,
                    format!("`{}` is a task handle", type_name(&return_type)),
                ))
                .with_help(
                    "an async method returns `Task<T>` automatically — annotate `T` (e.g. `-> i64`)",
                ),
            );
        }
        let previous_class = self.current_class.replace(class_name.to_string());
        let previous_async_context = self.current_async_context;
        let previous_static_method = self.in_static_method;
        self.current_async_context = m.is_async;
        self.in_static_method = m.is_static;
        self.current_return_type = return_type.clone();
        self.symbols.push_scope();
        let async_locals_before = self
            .async_local_types
            .keys()
            .copied()
            .collect::<HashSet<_>>();

        // Instance methods bind `self` to the enclosing class — implicitly, whether
        // or not the (legacy) explicit `self` parameter was written (willow-qsqf
        // §9.1). Static methods get no `self`; references to it there are E0831.
        if !m.is_static {
            let receiver_ty = Type::Named(class_name.to_string());
            self.symbols.define_var(
                "self".to_string(),
                VarInfo {
                    ty: receiver_ty.clone(),
                    mutable: false,
                    is_param: true,
                    declaration_span: m.span,
                },
            );
        }

        for (param, ty) in m.params.iter().zip(param_types.iter()) {
            self.symbols.define_var(
                param.name.clone(),
                VarInfo {
                    ty: ty.clone(),
                    mutable: matches!(&param.mode, ParamMode::Reference { mutable: true, .. }),
                    is_param: true,
                    declaration_span: param.span,
                },
            );
        }

        self.check_block(&m.body);
        if m.is_async {
            let task_params = if m.is_static {
                param_types.clone()
            } else {
                let mut task_params = Vec::with_capacity(param_types.len() + 1);
                task_params.push(Type::Named(class_name.to_string()));
                task_params.extend(param_types.iter().cloned());
                task_params
            };
            let locals = self
                .async_local_types
                .iter()
                .filter_map(|(span, ty)| {
                    (!async_locals_before.contains(span)).then_some(ty.clone())
                })
                .collect::<Vec<_>>();
            self.check_async_task_send(m.span, &return_type, &task_params, &locals);
        }
        self.symbols.pop_scope();
        self.current_class = previous_class;
        self.current_async_context = previous_async_context;
        self.in_static_method = previous_static_method;
    }

    pub(super) fn check_function(&mut self, f: &FunctionDecl) {
        let return_type = self.normalize_type(&f.return_type, f.span);
        let param_types = self.normalize_param_types(&f.params);
        self.validate_type(&return_type, f.span);
        for (param, ty) in f.params.iter().zip(param_types.iter()) {
            self.validate_type(ty, param.span);
        }
        // An async fn already returns `Task<ReturnType>`, so its declared return
        // type must be the awaited value, not a task handle — otherwise the call
        // would be a confusing nested `Task<Task<T>>` (willow-h2vf, case A → E0809).
        if f.is_async && is_task_handle_type(&return_type) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0809,
                    "async fn return type must be the awaited value, not a task handle",
                )
                .with_label(Label::primary(
                    f.span,
                    format!("`{}` is a task handle", type_name(&return_type)),
                ))
                .with_help(
                    "an async fn returns `Task<T>` automatically — annotate `T` (e.g. `-> i64`); \
                     use a `Channel<T>` to pass tasks",
                ),
            );
        }
        let previous_async_context = self.current_async_context;
        self.current_async_context = f.is_async;
        self.current_return_type = return_type.clone();
        self.symbols.push_scope();
        let async_locals_before = self
            .async_local_types
            .keys()
            .copied()
            .collect::<HashSet<_>>();
        for (param, ty) in f.params.iter().zip(param_types.iter()) {
            self.symbols.define_var(
                param.name.clone(),
                VarInfo {
                    ty: ty.clone(),
                    mutable: matches!(&param.mode, ParamMode::Reference { mutable: true, .. }),
                    is_param: true,
                    declaration_span: param.span,
                },
            );
        }
        self.check_block(&f.body);
        if f.is_async {
            let locals = self
                .async_local_types
                .iter()
                .filter_map(|(span, ty)| {
                    (!async_locals_before.contains(span)).then_some(ty.clone())
                })
                .collect::<Vec<_>>();
            self.check_async_task_send(f.span, &return_type, &param_types, &locals);
        }
        self.symbols.pop_scope();
        self.current_async_context = previous_async_context;
    }
}
