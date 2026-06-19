//! The `check_*` type-checking methods (extracted from `mod.rs`). `check_program`
//! stays `pub` (the entry point); the rest are `pub(super)`. As a child module
//! these reach `TypeChecker`'s private fields/methods.

use std::collections::{HashMap, HashSet};

use crate::diagnostics::{Diagnostic, ErrorCode, FixSuggestion, Label, Severity, Span};
#[cfg(test)]
use crate::lexer::Lexer;
#[cfg(test)]
use crate::parser::Parser;
use crate::parser::ast::*;
use crate::semantic::symbols::*;

use super::*;

impl TypeChecker {
    /// Report E2003 if `name` (a local declaration) collides with an imported
    /// name (a module access name or a directly imported item).
    pub(super) fn check_local_decl_collision(&mut self, name: &str, span: Span) {
        if let Some(import_span) = self.imported_names.get(name).copied() {
            let mut diag = Diagnostic::new(
                Severity::Error,
                ErrorCode::E2003,
                format!("name `{name}` is defined both by an import and a local declaration"),
            )
            .with_label(Label::primary(span, "local declaration here"));
            if let Some(s) = import_span {
                diag = diag.with_label(Label::secondary(s, "imported here"));
            }
            self.push(diag.with_help("rename the local declaration or the import"));
        }
    }

    pub fn check_program(&mut self, program: &Program) {
        self.register_std_imports(&program.imports);

        // Pass 1: register class shapes, enum declarations, and interfaces.
        // Interfaces share the top-level namespace with classes/enums/functions
        // and must be registered before class conformance is validated.
        for item in &program.items {
            match item {
                Item::Class(c) => {
                    self.check_local_decl_collision(&c.name, c.span);
                    self.register_class(c);
                }
                Item::Enum(e) => {
                    self.check_local_decl_collision(&e.name, e.span);
                    self.register_enum(e);
                }
                Item::Interface(i) => {
                    self.check_local_decl_collision(&i.name, i.span);
                    self.register_interface(i, None);
                }
                _ => {}
            }
        }

        // Pass 2: register all top-level function signatures
        for item in &program.items {
            if let Item::Function(f) = item {
                self.check_local_decl_collision(&f.name, f.span);
                let params = self.normalize_param_types(&f.params);
                let param_infos = self.normalize_param_infos(&f.params);
                let return_type = self.normalize_type(&f.return_type, f.span);
                self.symbols.define_func(
                    f.name.clone(),
                    FuncInfo {
                        param_infos,
                        params,
                        return_type,
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
                Item::Enum(_) => {} // already registered
                Item::Interface(i) => self.check_interface(i), // validate `extends`
            }
        }
    }

    /// Validate an interface's `extends` clause (willow-1js.2 / willow-1js.8):
    /// each super must be a (single) registered interface, with no cycle.
    pub(super) fn check_interface(&mut self, decl: &InterfaceDecl) {
        // v1 supports a single super-interface: a sub-interface's vtable is laid
        // out to be compatible with ONE super, so multiple supers cannot all be
        // dispatched correctly yet.
        if decl.extends.len() > 1 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0424,
                    format!(
                        "interface `{}` extends {} interfaces; only one is supported",
                        decl.name,
                        decl.extends.len()
                    ),
                )
                .with_label(Label::primary(decl.span, "multiple super-interfaces"))
                .with_help("extend a single interface for now"),
            );
        }
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
    fn check_interface_default_body(&mut self, m: &InterfaceMethodDecl, iface_name: &str) {
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
                let params = self
                    .implicit_constructor_fields(&base_name)
                    .iter()
                    .map(|(_, ty)| ty.clone())
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
            None => self
                .implicit_constructor_fields(&resolved)
                .iter()
                .map(|(_, t)| t.clone())
                .collect(),
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

    pub(super) fn check_block(&mut self, block: &Block) {
        self.symbols.push_scope();
        self.narrowed_vars.push(HashMap::new());
        for stmt in &block.stmts {
            self.check_stmt(stmt);
        }
        self.narrowed_vars.pop();
        self.symbols.pop_scope();
    }

    pub(super) fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => {
                let annotation = s.ty.as_ref().map(|ty| self.normalize_type(ty, s.span));
                // A `let xs: Array<I> = [..]` literal is checked element-wise
                // against `I`, so classes implementing interface `I` are accepted.
                let inferred = match (&annotation, &s.init) {
                    (Some(Type::Array(elem)), Expr::ArrayLiteral(elements, lit_span)) => {
                        self.check_array_literal_expecting(elements, *lit_span, Some(elem.as_ref()))
                    }
                    _ => self.check_expr(&s.init),
                };
                let ty = if let Some(ann) = &annotation {
                    self.validate_type(ann, s.span);
                    let channel_new_infers_from_annotation =
                        channel_element_type(ann).is_some() && is_untyped_channel_new_call(&s.init);
                    if !channel_new_infers_from_annotation && !self.types_compatible(ann, &inferred)
                    {
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
                    } else if let Some(diag) = self.unresolved_generic_enum_diagnostic(
                        &s.init,
                        &inferred,
                        s.init.span(),
                        &s.name,
                    ) {
                        self.push(diag);
                    }
                    inferred
                };
                // Record the resolved type of locals inside async fns so the
                // backend can frame-back unannotated live-across-await locals
                // (willow-lpn.5c).
                if self.current_async_context {
                    self.async_local_types.insert(s.span, ty.clone());
                }
                // `_` is a wildcard: evaluate the initializer for side effects but do
                // not bind a variable (allows multiple `let _ = expr;` in the same scope).
                if s.name == "_" {
                    return;
                }
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
            Stmt::FieldAssign(s) => {
                let obj_ty = self.check_expr(&s.object);
                let field_ty = self.resolve_field(&obj_ty, &s.field, s.span, true);
                let val_ty = self.check_expr(&s.value);
                if field_ty != Type::Void && !self.types_compatible(&field_ty, &val_ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(&field_ty, &val_ty),
                            format!(
                                "mismatched types: expected `{}`, found `{}`",
                                type_name(&field_ty),
                                type_name(&val_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            s.span,
                            format!("expected `{}`", type_name(&field_ty)),
                        )),
                    );
                }
            }
            Stmt::StaticFieldAssign(s) => self.check_static_field_assign(s),
            Stmt::SuperInit(s) => self.check_super_init(s),
            Stmt::IndexAssign(s) => {
                let arr_ty = self.check_expr(&s.array);
                let idx_ty = self.check_expr(&s.index);
                if !matches!(idx_ty, Type::I64) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("array index must be `i64`, found `{}`", type_name(&idx_ty)),
                        )
                        .with_label(Label::primary(s.index.span(), "index is not an `i64`")),
                    );
                }
                let val_ty = self.check_expr(&s.value);
                match &arr_ty {
                    Type::Array(elem) => {
                        if !self.types_compatible(elem, &val_ty) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    self.type_mismatch_error_code(elem, &val_ty),
                                    format!(
                                        "cannot assign `{}` to an element of `Array<{}>`",
                                        type_name(&val_ty),
                                        type_name(elem)
                                    ),
                                )
                                .with_label(Label::primary(
                                    s.span,
                                    format!("expected `{}`", type_name(elem)),
                                )),
                            );
                        }
                    }
                    Type::Void => {}
                    other => {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!("cannot index a value of type `{}`", type_name(other)),
                            )
                            .with_label(Label::primary(s.span, "not an array")),
                        );
                    }
                }
            }
            Stmt::Assign(s) => {
                if s.name == "this" {
                    self.push_legacy_this_error(s.span);
                    return;
                }
                // Reject direct assignment to `self`.
                if s.name == "self" {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0552,
                            format!("cannot assign to `{}`", s.name),
                        )
                        .with_label(Label::primary(s.span, "cannot assign to receiver"))
                        .with_help(format!("to mutate fields, use `{}.field = value`", s.name)),
                    );
                    return;
                }
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
            Stmt::For(s) => {
                let iterable_ty = self.check_expr(&s.iterable);
                let elem_ty = match &iterable_ty {
                    Type::Array(elem) => (**elem).clone(),
                    Type::Generic(name, args)
                        if name == "Range" && args.as_slice() == [Type::I64] =>
                    {
                        Type::I64
                    }
                    Type::Void => Type::Void,
                    other => {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!("cannot iterate over `{}`", type_name(other)),
                            )
                            .with_label(Label::primary(
                                s.iterable.span(),
                                "for-in requires an array or i64 range",
                            ))
                            .with_help(
                                "use `for item in array { ... }` with `Array<T>` or `for n in start..end { ... }`",
                            ),
                        );
                        Type::Void
                    }
                };

                if self.current_async_context {
                    let iter_slot_ty = if is_i64_range_type(&iterable_ty) {
                        Type::I64
                    } else {
                        iterable_ty.clone()
                    };
                    self.async_local_types
                        .insert(s.iter_frame_key(), iter_slot_ty);
                    self.async_local_types
                        .insert(s.index_frame_key(), Type::I64);
                    if s.name != "_" {
                        self.async_local_types.insert(s.name_span, elem_ty.clone());
                    }
                }

                self.symbols.push_scope();
                self.narrowed_vars.push(HashMap::new());
                if s.name != "_" {
                    self.symbols.define_var(
                        s.name.clone(),
                        VarInfo {
                            ty: elem_ty,
                            mutable: false,
                            is_param: false,
                            declaration_span: s.name_span,
                        },
                    );
                }
                for stmt in &s.body.stmts {
                    self.check_stmt(stmt);
                }
                self.narrowed_vars.pop();
                self.symbols.pop_scope();
            }
            Stmt::Return(s) => {
                // In a constructor, a bare `return;` is fine but `return <value>`
                // is rejected (willow-scq2 §8 → E0841).
                if self.in_constructor {
                    if let Some(v) = &s.value {
                        self.check_expr(v);
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0841,
                                "constructor `init` cannot return a value",
                            )
                            .with_label(Label::primary(s.span, "remove the returned value"))
                            .with_help("a constructor implicitly returns the new object"),
                        );
                    }
                    return;
                }
                // `return Result::Ok();` (zero-arg) is the success value of a
                // `Result<void, E>` function: the Ok payload is void, so no
                // argument is required (willow-exg).
                if let Some(Expr::StaticCall(sc)) = &s.value {
                    let returns_result_void = matches!(
                        &self.current_return_type,
                        Type::Generic(n, args)
                            if n == "Result" && args.len() == 2 && args[0] == Type::Void
                    );
                    if returns_result_void
                        && sc.class == "Result"
                        && sc.method == "Ok"
                        && sc.args.is_empty()
                    {
                        return;
                    }
                }
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

    pub(super) fn check_expr(&mut self, expr: &Expr) -> Type {
        match expr {
            Expr::Integer(_, _) => Type::I64,
            Expr::Float(_, _) => Type::F64,
            Expr::Bool(_, _) => Type::Bool,
            Expr::Nil(_) => Type::Nil,
            Expr::String(_, _) => Type::String,
            Expr::Var(name, span) => {
                if name == "this" {
                    self.push_legacy_this_error(*span);
                    return Type::Void;
                }
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
                // Give a specialized error for receiver keywords used outside instance methods.
                if name == "self" {
                    let diag = if self.in_static_method {
                        let where_ = self
                            .current_class
                            .as_deref()
                            .map(|c| format!(" `{}`", c))
                            .unwrap_or_default();
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0831,
                            format!("`self` is not available in static method{}", where_),
                        )
                        .with_label(Label::primary(*span, "`self` in a static method"))
                        .with_help(
                            "static methods have no receiver; use an instance method instead",
                        )
                    } else if self.in_static_initializer {
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0837,
                            "`self` is not available in a static property initializer",
                        )
                        .with_label(Label::primary(*span, "`self` in static initializer"))
                        .with_help("static initializers run before any instance exists")
                    } else {
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0550,
                            "`self` can only be used inside an instance method",
                        )
                        .with_label(Label::primary(*span, "`self` used outside instance method"))
                        .with_help(
                            "declare the method without `static` to make it an instance method",
                        )
                    };
                    self.push(diag);
                    return Type::Void;
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
                    // Calling an async fn captures its arguments into a Task that
                    // may cross a worker boundary — enforce Send/Sync (dgwo.4).
                    if info.is_async {
                        self.check_async_capture(&info.param_infos, &c.args);
                    }
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
                // `.` is instance member access; module items use `::`. Using
                // `math.add(..)` on a module is an error that points at `::`.
                if let Expr::Var(name, _) = &m.object {
                    if self.symbols.lookup_var(name).is_none()
                        && self.symbols.lookup_module(name).is_some()
                    {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0350,
                                format!("`{name}` is a module; use `::` to access its items"),
                            )
                            .with_label(Label::primary(m.span, "module accessed with `.`"))
                            .with_help(format!(
                                "write `{name}::{method}(...)` instead of `{name}.{method}(...)`",
                                method = m.method
                            )),
                        );
                        return Type::Void;
                    }
                }
                let obj_ty = self.check_expr(&m.object);
                if let Some(ret) = self.check_option_result_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_concurrency_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_array_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_frozen_array_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_map_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_frozen_map_method_call(&obj_ty, m) {
                    return ret;
                }
                let ret = self.resolve_method(&obj_ty, &m.method, &m.args, m.span);
                ret
            }
            Expr::StaticCall(s) => {
                self.resolve_static_call(&s.class, &s.type_args, &s.method, &s.args, s.span)
            }
            Expr::StaticField(s) => self.resolve_static_field_read(&s.class, &s.field, s.span),
            Expr::New(n) => self.check_new(n),
            Expr::ObjectLiteral(o) => self.check_object_literal(o),
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
                    Type::Generic(name, mut args)
                        if (name == "Future" || name == "Task") && args.len() == 1 =>
                    {
                        args.remove(0)
                    }
                    other => {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0803,
                                format!("cannot await value of type `{}`", type_name(&other)),
                            )
                            .with_label(Label::primary(a.expr.span(), "expected an awaitable"))
                            .with_help(
                                "await only `Task<T>` values returned by async functions or `Future<T>` runtime APIs",
                            ),
                        );
                        Type::Void
                    }
                }
            }
            Expr::Select(s) => {
                self.check_select(s);
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
            Expr::Range(r) => self.check_range(r),
            Expr::Lambda(l) => self.check_lambda(l),
            Expr::Match(m) => self.check_match_expr(m),
            Expr::TryPropagate(inner, span) => self.check_try_propagate(inner, *span),
            Expr::ArrayLiteral(elements, span) => self.check_array_literal(elements, *span),
            Expr::Index(arr, index, span) => self.check_index(arr, index, *span),
        }
    }

    pub(super) fn check_range(&mut self, range: &RangeExpr) -> Type {
        // A range is a first-class `Range<i64>` value. Its bounds are checked
        // normally; a nested range bound surfaces as a non-`i64` bound below.
        let start_ty = self.check_expr(&range.start);
        let end_ty = self.check_expr(&range.end);

        if start_ty != Type::I64 || end_ty != Type::I64 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "range bounds must be `i64`, found `{}` and `{}`",
                        type_name(&start_ty),
                        type_name(&end_ty)
                    ),
                )
                .with_label(Label::primary(range.span, "range bounds must be `i64`")),
            );
        }

        range_type()
    }

    /// Type-check an array literal `[e0, e1, ...]`. The element type is inferred
    /// from the first element; all elements must agree. An empty literal yields
    /// `Array<Void>`, an unresolved placeholder that a type annotation resolves
    /// (e.g. `let xs: Array<i64> = [];`).
    pub(super) fn check_array_literal(&mut self, elements: &[Expr], span: Span) -> Type {
        self.check_array_literal_expecting(elements, span, None)
    }

    /// Type-check an array literal. When `expected_elem` is given (e.g. from a
    /// `let xs: Array<Animal> = [...]` annotation), each element is checked
    /// against it — this allows a heterogeneous literal of classes that all
    /// implement the same interface, and the literal takes the expected type.
    pub(super) fn check_array_literal_expecting(
        &mut self,
        elements: &[Expr],
        _span: Span,
        expected_elem: Option<&Type>,
    ) -> Type {
        if let Some(expected) = expected_elem {
            for el in elements {
                let ty = self.check_expr(el);
                if !self.types_compatible(expected, &ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(expected, &ty),
                            format!(
                                "array element expects `{}`, found `{}`",
                                type_name(expected),
                                type_name(&ty)
                            ),
                        )
                        .with_label(Label::primary(el.span(), "mismatched element type")),
                    );
                }
            }
            return Type::Array(Box::new(expected.clone()));
        }

        if elements.is_empty() {
            return Type::Array(Box::new(Type::Void));
        }
        let first_ty = self.check_expr(&elements[0]);
        for el in elements.iter().skip(1) {
            let ty = self.check_expr(el);
            if !self.types_compatible(&first_ty, &ty) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "array elements must have the same type: expected `{}`, found `{}`",
                            type_name(&first_ty),
                            type_name(&ty)
                        ),
                    )
                    .with_label(Label::primary(el.span(), "mismatched element type")),
                );
            }
        }
        Type::Array(Box::new(first_ty))
    }

    /// Type-check an index expression `arr[index]`. `arr` must be `Array<T>` and
    /// `index` must be `i64`; the result type is `T`.
    pub(super) fn check_index(&mut self, arr: &Expr, index: &Expr, span: Span) -> Type {
        let arr_ty = self.check_expr(arr);
        let idx_ty = self.check_expr(index);
        if !matches!(idx_ty, Type::I64) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!("array index must be `i64`, found `{}`", type_name(&idx_ty)),
                )
                .with_label(Label::primary(index.span(), "index is not an `i64`")),
            );
        }
        match &arr_ty {
            Type::Array(elem) => (**elem).clone(),
            // Read-only indexing of an immutable `FrozenArray<T>` (willow-dgwo.7).
            Type::Generic(name, args) if name == "FrozenArray" && args.len() == 1 => {
                args[0].clone()
            }
            // Recover quietly from an earlier error that produced Void.
            Type::Void => Type::Void,
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("cannot index a value of type `{}`", type_name(other)),
                    )
                    .with_label(Label::primary(span, "not an array"))
                    .with_help("indexing with `[..]` requires an `Array<T>` or `FrozenArray<T>`"),
                );
                Type::Void
            }
        }
    }

    /// Builtin methods on `Array<T>`. Returns `Some(ret)` when `obj_ty` is an
    /// array (handling the method or reporting an unknown one), `None` otherwise.
    pub(super) fn check_array_method_call(
        &mut self,
        obj_ty: &Type,
        m: &MethodCallExpr,
    ) -> Option<Type> {
        let Type::Array(elem) = obj_ty else {
            return None;
        };
        match m.method.as_str() {
            "len" => {
                if !m.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "`Array::len` takes no arguments",
                        )
                        .with_label(Label::primary(m.span, "unexpected arguments")),
                    );
                }
                Some(Type::I64)
            }
            "push" => {
                let elem_ty = (**elem).clone();
                if m.args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("`Array::push` expects 1 argument, got {}", m.args.len()),
                        )
                        .with_label(Label::primary(m.span, "expected `push(value)`")),
                    );
                } else {
                    let v = self.check_expr(&m.args[0].expr);
                    if !self.types_compatible(&elem_ty, &v) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "cannot push `{}` to `Array<{}>`",
                                    type_name(&v),
                                    type_name(&elem_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                m.args[0].expr.span(),
                                "wrong element type",
                            )),
                        );
                    }
                }
                Some(Type::Void)
            }
            "pop" => {
                if !m.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "`Array::pop` takes no arguments",
                        )
                        .with_label(Label::primary(m.span, "unexpected arguments")),
                    );
                }
                Some((**elem).clone())
            }
            // `.freeze()` -> an immutable `FrozenArray<T>` copy that is Sync when
            // T is Sync, so it can be shared across tasks (willow-dgwo.7).
            "freeze" => {
                if !m.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "`Array::freeze` takes no arguments",
                        )
                        .with_label(Label::primary(m.span, "unexpected arguments")),
                    );
                }
                Some(Type::Generic(
                    "FrozenArray".to_string(),
                    vec![(**elem).clone()],
                ))
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("no method `{}` on `Array<{}>`", other, type_name(elem)),
                    )
                    .with_label(Label::primary(m.span, "unknown array method"))
                    .with_help(
                        "arrays support `.len()`, `.push(v)`, `.pop()`, `.freeze()`, and indexing `arr[i]`",
                    ),
                );
                Some(Type::Void)
            }
        }
    }

    /// Builtin methods on the immutable `FrozenArray<T>` (willow-dgwo.7): only
    /// `.len()` plus read-only indexing `fa[i]`; mutation methods are rejected.
    pub(super) fn check_frozen_array_method_call(
        &mut self,
        obj_ty: &Type,
        m: &MethodCallExpr,
    ) -> Option<Type> {
        let Type::Generic(name, args) = obj_ty else {
            return None;
        };
        if name != "FrozenArray" || args.len() != 1 {
            return None;
        }
        match m.method.as_str() {
            "len" => Some(Type::I64),
            "push" | "pop" | "set" => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("`FrozenArray` is immutable; `{}` is not allowed", m.method),
                    )
                    .with_label(Label::primary(m.span, "frozen arrays cannot be mutated"))
                    .with_help(
                        "freeze a copy of a mutable `Array<T>`; read it with `[i]` / `.len()`",
                    ),
                );
                Some(Type::Void)
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "no method `{other}` on `FrozenArray<{}>`",
                            type_name(&args[0])
                        ),
                    )
                    .with_label(Label::primary(m.span, "unknown method"))
                    .with_help("`FrozenArray` supports `.len()` and indexing `fa[i]`"),
                );
                Some(Type::Void)
            }
        }
    }

    /// Builtin methods on `Map<K, V>`: `insert(k, v)`, `get(k) -> Option<V>`,
    /// `contains(k) -> bool`, `len() -> i64`. Returns `Some(ret)` when `obj_ty`
    /// is a map, `None` otherwise.
    pub(super) fn check_map_method_call(
        &mut self,
        obj_ty: &Type,
        m: &MethodCallExpr,
    ) -> Option<Type> {
        let Type::Generic(name, args) = obj_ty else {
            return None;
        };
        if name != "Map" || args.len() != 2 {
            return None;
        }
        let key_ty = args[0].clone();
        let val_ty = args[1].clone();

        let check_key = |checker: &mut Self, arg: &CallArg| {
            let k = checker.check_expr(&arg.expr);
            if key_ty != Type::Void && !checker.types_compatible(&key_ty, &k) {
                checker.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "map key type mismatch: expected `{}`, found `{}`",
                            type_name(&key_ty),
                            type_name(&k)
                        ),
                    )
                    .with_label(Label::primary(arg.expr.span(), "wrong key type")),
                );
            }
        };

        match m.method.as_str() {
            "insert" => {
                if m.args.len() != 2 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("`Map::insert` expects 2 arguments, got {}", m.args.len()),
                        )
                        .with_label(Label::primary(m.span, "expected `insert(key, value)`")),
                    );
                } else {
                    check_key(self, &m.args[0]);
                    let v = self.check_expr(&m.args[1].expr);
                    if val_ty != Type::Void && !self.types_compatible(&val_ty, &v) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "map value type mismatch: expected `{}`, found `{}`",
                                    type_name(&val_ty),
                                    type_name(&v)
                                ),
                            )
                            .with_label(Label::primary(m.args[1].expr.span(), "wrong value type")),
                        );
                    }
                }
                Some(Type::Void)
            }
            "get" => {
                if m.args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("`Map::get` expects 1 argument, got {}", m.args.len()),
                        )
                        .with_label(Label::primary(m.span, "expected `get(key)`")),
                    );
                } else {
                    check_key(self, &m.args[0]);
                }
                Some(Type::Generic("Option".to_string(), vec![val_ty]))
            }
            "contains" => {
                if m.args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("`Map::contains` expects 1 argument, got {}", m.args.len()),
                        )
                        .with_label(Label::primary(m.span, "expected `contains(key)`")),
                    );
                } else {
                    check_key(self, &m.args[0]);
                }
                Some(Type::Bool)
            }
            "len" => {
                if !m.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "`Map::len` takes no arguments",
                        )
                        .with_label(Label::primary(m.span, "unexpected arguments")),
                    );
                }
                Some(Type::I64)
            }
            // `.freeze()` -> an immutable `FrozenMap<K,V>` copy, Sync when K,V are
            // Sync, so it can be shared across tasks (willow-dgwo.10).
            "freeze" => {
                if !m.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "`Map::freeze` takes no arguments",
                        )
                        .with_label(Label::primary(m.span, "unexpected arguments")),
                    );
                }
                Some(Type::Generic("FrozenMap".to_string(), vec![key_ty, val_ty]))
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "no method `{}` on `Map<{}, {}>`",
                            other,
                            type_name(&key_ty),
                            type_name(&val_ty)
                        ),
                    )
                    .with_label(Label::primary(m.span, "unknown map method"))
                    .with_help(
                        "maps support `.insert(k, v)`, `.get(k)`, `.contains(k)`, `.len()`, `.freeze()`",
                    ),
                );
                Some(Type::Void)
            }
        }
    }

    /// Builtin methods on the immutable `FrozenMap<K, V>` (willow-dgwo.10):
    /// read-only `.get(k) -> Option<V>`, `.contains(k) -> bool`, `.len() -> i64`;
    /// `insert`/`remove` are rejected.
    pub(super) fn check_frozen_map_method_call(
        &mut self,
        obj_ty: &Type,
        m: &MethodCallExpr,
    ) -> Option<Type> {
        let Type::Generic(name, args) = obj_ty else {
            return None;
        };
        if name != "FrozenMap" || args.len() != 2 {
            return None;
        }
        let key_ty = args[0].clone();
        let val_ty = args[1].clone();
        if let Some(arg) = m.args.first() {
            let k = self.check_expr(&arg.expr);
            if matches!(m.method.as_str(), "get" | "contains")
                && key_ty != Type::Void
                && !self.types_compatible(&key_ty, &k)
            {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "map key type mismatch: expected `{}`, found `{}`",
                            type_name(&key_ty),
                            type_name(&k)
                        ),
                    )
                    .with_label(Label::primary(arg.expr.span(), "wrong key type")),
                );
            }
        }
        match m.method.as_str() {
            "get" => Some(Type::Generic("Option".to_string(), vec![val_ty])),
            "contains" => Some(Type::Bool),
            "len" => Some(Type::I64),
            "insert" | "remove" => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("`FrozenMap` is immutable; `{}` is not allowed", m.method),
                    )
                    .with_label(Label::primary(m.span, "frozen maps cannot be mutated"))
                    .with_help("freeze a copy of a mutable `Map<K, V>`; read it with `.get`/`.contains`/`.len`"),
                );
                Some(Type::Void)
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "no method `{other}` on `FrozenMap<{}, {}>`",
                            type_name(&key_ty),
                            type_name(&val_ty)
                        ),
                    )
                    .with_label(Label::primary(m.span, "unknown method"))
                    .with_help("`FrozenMap` supports `.get(k)`, `.contains(k)`, `.len()`"),
                );
                Some(Type::Void)
            }
        }
    }

    pub(super) fn check_try_propagate(&mut self, inner: &Expr, span: Span) -> Type {
        let operand_ty = self.check_expr(inner);

        if let Type::Generic(name, args) = &operand_ty {
            if name == "Option" && args.len() == 1 {
                let some_ty = args[0].clone();
                let return_ty = self.current_return_type.clone();
                match &return_ty {
                    Type::Generic(ret_name, ret_args)
                        if ret_name == "Option" && ret_args.len() == 1 =>
                    {
                        return some_ty;
                    }
                    other => {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1807,
                                format!(
                                    "`?` on `Option<T>` can only be used inside a function returning `Option<U>`, found `{}`",
                                    type_name(other)
                                ),
                            )
                            .with_label(Label::primary(span, "invalid context for Option `?`"))
                            .with_help("change the function return type to `Option<U>`"),
                        );
                        return some_ty;
                    }
                }
            }
        }

        // Otherwise the operand must be Result<T,E>.
        let (ok_ty, err_ty) = match &operand_ty {
            Type::Generic(name, args) if name == "Result" && args.len() == 2 => {
                (args[0].clone(), args[1].clone())
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1806,
                        format!(
                            "the `?` operator requires `Result<T,E>` or `Option<T>`, found `{}`",
                            type_name(other)
                        ),
                    )
                    .with_label(Label::primary(span, "not a Result or Option"))
                    .with_help(
                        "wrap the value in `Result::Ok(...)`, `Result::Err(...)`, or `Option::Some(...)`",
                    ),
                );
                return Type::Void;
            }
        };

        // The enclosing function must return Result<U,E> with matching error type
        let return_ty = self.current_return_type.clone();
        match &return_ty {
            Type::Generic(name, args) if name == "Result" && args.len() == 2 => {
                if args[1] == err_ty || args[1] == Type::Void || err_ty == Type::Void {
                    // ok_ty is the success value type
                    ok_ty
                } else if self.err_converts_via_into(&err_ty, &args[1]) {
                    // Automatic error conversion (willow-1ow): the operand error
                    // `E1` implements `Into<E2>`, so `?` converts `E1 -> E2` on
                    // the Err early-return path. Codegen emits `e1.into()`.
                    ok_ty
                } else {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E1805,
                            format!(
                                "error type mismatch: function returns `Result<_, {}>` but `?` propagates `{}`",
                                type_name(&args[1]),
                                type_name(&err_ty)
                            ),
                        )
                        .with_label(Label::primary(span, "error type mismatch"))
                        .with_help(format!(
                            "implement `Into<{}>` on `{}` to allow `?` to convert the error",
                            type_name(&args[1]),
                            type_name(&err_ty)
                        )),
                    );
                    ok_ty
                }
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1807,
                        format!(
                            "`?` can only be used inside a function returning `Result<T,E>`, found `{}`",
                            type_name(other)
                        ),
                    )
                    .with_label(Label::primary(span, "invalid context for `?`"))
                    .with_help("change the function return type to `Result<T, E>`"),
                );
                ok_ty
            }
        }
    }

    pub(super) fn check_lambda(&mut self, l: &LambdaExpr) -> Type {
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

        // Record the inferred return type so the backend can use it without
        // falling back to I64 when no explicit annotation is present.
        self.lambda_return_types.insert(l.span, ret_ty.clone());

        Type::Fn(param_types, Box::new(ret_ty))
    }

    pub(super) fn check_match_expr(&mut self, m: &MatchExpr) -> Type {
        let scrutinee_ty = self.check_expr(&m.scrutinee);

        if m.arms.is_empty() {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1202,
                    "match expression has no arms",
                )
                .with_label(Label::primary(m.span, "no arms in match")),
            );
            return Type::Void;
        }

        let mut covered_variants: HashSet<String> = HashSet::new();
        let mut has_wildcard = false;
        let mut has_true = false;
        let mut has_false = false;
        let mut result_type: Option<Type> = None;
        let mut found_unreachable = false;

        for arm in &m.arms {
            // Check if arm is unreachable (after a wildcard/binding)
            if has_wildcard && !found_unreachable {
                self.push(
                    Diagnostic::new(Severity::Warning, ErrorCode::W1201, "unreachable match arm")
                        .with_label(Label::primary(arm.span, "this arm is unreachable")),
                );
                found_unreachable = true;
            }

            // Validate pattern and track coverage
            match &arm.pattern {
                Pattern::Wildcard(_) => {
                    has_wildcard = true;
                }
                Pattern::Binding { .. } => {
                    has_wildcard = true; // binding covers everything
                }
                Pattern::LiteralBool(b, span) => {
                    if scrutinee_ty != Type::Bool {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1205,
                                format!(
                                    "bool pattern cannot match scrutinee of type `{}`",
                                    type_name(&scrutinee_ty)
                                ),
                            )
                            .with_label(Label::primary(*span, "pattern type mismatch")),
                        );
                    }
                    if *b {
                        has_true = true;
                    } else {
                        has_false = true;
                    }
                }
                Pattern::LiteralInt(_, span) => {
                    if scrutinee_ty != Type::I64 {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1205,
                                format!(
                                    "integer pattern cannot match scrutinee of type `{}`",
                                    type_name(&scrutinee_ty)
                                ),
                            )
                            .with_label(Label::primary(*span, "pattern type mismatch")),
                        );
                    }
                }
                Pattern::EnumVariant {
                    enum_name,
                    variant,
                    span,
                } => {
                    // Generic enum variant patterns: the scrutinee may be
                    // Generic(enum_name, type_args) rather than Named(enum_name).
                    let is_builtin_match = matches!(&scrutinee_ty,
                        Type::Generic(n, _) if n == enum_name
                    );
                    // Verify enum_name matches scrutinee type
                    if !is_builtin_match {
                        match &scrutinee_ty {
                            Type::Named(sname) if sname == enum_name => {}
                            _ => {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E1205,
                                        format!(
                                            "enum pattern `{}::{}` cannot match scrutinee of type `{}`",
                                            enum_name,
                                            variant,
                                            type_name(&scrutinee_ty)
                                        ),
                                    )
                                    .with_label(Label::primary(*span, "pattern type mismatch")),
                                );
                            }
                        }
                        // Verify variant exists
                        let variant_valid = self
                            .symbols
                            .lookup_enum(enum_name)
                            .and_then(|e| e.variants.iter().find(|v| v.name == *variant))
                            .is_some();
                        if !variant_valid {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E1208,
                                    format!("no variant `{}` in enum `{}`", variant, enum_name),
                                )
                                .with_label(Label::primary(*span, "unknown enum variant")),
                            );
                        }
                    }
                    covered_variants.insert(variant.clone());
                }
                Pattern::EnumVariantTuple {
                    enum_name,
                    variant,
                    bindings,
                    span,
                } => {
                    // Generic enum variant: resolve concrete payload types from scrutinee.
                    let builtin_payload: Option<Vec<Type>> =
                        self.resolve_generic_variant_payload(enum_name, variant, &scrutinee_ty);

                    if let Some(ref pts) = builtin_payload {
                        // Built-in generic variant — validate binding count
                        if bindings.len() != pts.len() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E1209,
                                    format!(
                                        "variant `{}::{}` expects {} field(s), found {}",
                                        enum_name,
                                        variant,
                                        pts.len(),
                                        bindings.len()
                                    ),
                                )
                                .with_label(Label::primary(*span, "wrong number of bindings")),
                            );
                        }
                    } else {
                        // User-defined enum variant
                        match &scrutinee_ty {
                            Type::Named(sname) if sname == enum_name => {}
                            _ => {
                                self.push(Diagnostic::new(Severity::Error, ErrorCode::E1205,
                                    format!("enum pattern `{}::{}(..)` cannot match scrutinee of type `{}`",
                                        enum_name, variant, type_name(&scrutinee_ty)))
                                    .with_label(Label::primary(*span, "pattern type mismatch")));
                            }
                        }
                        let payload_types = self
                            .symbols
                            .lookup_enum(enum_name)
                            .and_then(|e| e.variants.iter().find(|v| v.name == *variant))
                            .map(|v| v.payload_types.clone());
                        match payload_types {
                            None => {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E1208,
                                        format!("no variant `{}` in enum `{}`", variant, enum_name),
                                    )
                                    .with_label(Label::primary(*span, "unknown enum variant")),
                                );
                            }
                            Some(ref pts) => {
                                if pts.is_empty() {
                                    self.push(
                                        Diagnostic::new(
                                            Severity::Error,
                                            ErrorCode::E1209,
                                            format!(
                                                "variant `{}::{}` has no payload; remove `(..)`",
                                                enum_name, variant
                                            ),
                                        )
                                        .with_label(
                                            Label::primary(
                                                *span,
                                                "fieldless variant used with payload pattern",
                                            ),
                                        ),
                                    );
                                } else if bindings.len() != pts.len() {
                                    self.push(
                                        Diagnostic::new(
                                            Severity::Error,
                                            ErrorCode::E1209,
                                            format!(
                                                "variant `{}::{}` expects {} field(s), found {}",
                                                enum_name,
                                                variant,
                                                pts.len(),
                                                bindings.len()
                                            ),
                                        )
                                        .with_label(
                                            Label::primary(*span, "wrong number of bindings"),
                                        ),
                                    );
                                }
                            }
                        }
                    }
                    covered_variants.insert(variant.clone());
                }
                Pattern::ClassDowncast {
                    class_name, span, ..
                } => {
                    // `Dog(d)` downcasts an interface scrutinee to a concrete
                    // class. The scrutinee must be an interface, and the class
                    // must implement it (else the arm can never match).
                    // Class patterns do not contribute to exhaustiveness, so a
                    // wildcard arm is still required.
                    let scrut_is_interface = matches!(&scrutinee_ty,
                        Type::Named(n) | Type::Generic(n, _)
                            if self.symbols.lookup_interface(n).is_some());
                    if !scrut_is_interface {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1205,
                                format!(
                                    "class pattern `{}(..)` requires an interface scrutinee, found `{}`",
                                    class_name,
                                    type_name(&scrutinee_ty)
                                ),
                            )
                            .with_label(Label::primary(*span, "scrutinee is not an interface"))
                            .with_help("match on a value of interface type to downcast to a class"),
                        );
                    } else if self.symbols.lookup_class(class_name).is_none() {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0350,
                                format!("cannot find class `{class_name}`"),
                            )
                            .with_label(Label::primary(*span, "unknown class in pattern")),
                        );
                    } else if !self.class_implements_interface(class_name, &scrutinee_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0415,
                                format!(
                                    "class `{}` does not implement `{}`, so this pattern can never match",
                                    class_name,
                                    type_name(&scrutinee_ty)
                                ),
                            )
                            .with_label(Label::primary(*span, "unrelated class")),
                        );
                    }
                }
            }

            // Check arm body in a new scope
            self.symbols.push_scope();
            // For EnumVariantTuple: bind payload variables in arm scope
            if let Pattern::EnumVariantTuple {
                enum_name,
                variant,
                bindings,
                ..
            } = &arm.pattern
            {
                // Resolve payload types: first check built-in generic types
                // Resolve concrete payload types: use generic instantiation when available.
                let payload_types: Vec<Type> = self
                    .resolve_generic_variant_payload(enum_name, variant, &scrutinee_ty)
                    .unwrap_or_default();
                for (binding, ty) in bindings.iter().zip(payload_types.iter()) {
                    self.symbols.define_var(
                        binding.clone(),
                        VarInfo {
                            ty: ty.clone(),
                            mutable: false,
                            is_param: false,
                            declaration_span: arm.pattern.span(),
                        },
                    );
                }
            }
            // For a class downcast pattern, bind the downcast value as the
            // concrete class (willow-1js.4). `_` does not bind.
            if let Pattern::ClassDowncast {
                class_name,
                binding,
                span: bspan,
            } = &arm.pattern
                && binding != "_"
            {
                self.symbols.define_var(
                    binding.clone(),
                    VarInfo {
                        ty: Type::Named(class_name.clone()),
                        mutable: false,
                        is_param: false,
                        declaration_span: *bspan,
                    },
                );
            }
            // For binding patterns, define the variable
            if let Pattern::Binding { name, span: bspan } = &arm.pattern {
                self.symbols.define_var(
                    name.clone(),
                    VarInfo {
                        ty: scrutinee_ty.clone(),
                        mutable: false,
                        is_param: false,
                        declaration_span: *bspan,
                    },
                );
            }
            let arm_ty = self.check_match_body(&arm.body);
            self.symbols.pop_scope();

            // Never arms don't constrain result type
            if arm_ty == Type::Never {
                continue;
            }

            // When the new arm type is a partial-generic (e.g. `Option<void>` from `None`),
            // keep the richer type already recorded rather than replacing it.
            let arm_ty = match (&result_type, &arm_ty) {
                (Some(existing), arm) if self.generic_partially_matches(existing, arm) => {
                    existing.clone()
                }
                (Some(existing), arm) if self.generic_partially_matches(arm, existing) => {
                    arm.clone()
                }
                _ => arm_ty,
            };

            match &result_type {
                None => result_type = Some(arm_ty),
                Some(existing) => {
                    if !self.types_compatible(existing, &arm_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1201,
                                format!(
                                    "match arms have incompatible types: `{}` and `{}`",
                                    type_name(existing),
                                    type_name(&arm_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                arm.span,
                                format!("found `{}`", type_name(&arm_ty)),
                            )),
                        );
                    }
                }
            }
        }

        // Exhaustiveness check
        if !has_wildcard {
            match &scrutinee_ty {
                Type::Bool => {
                    if !has_true || !has_false {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1207,
                                "non-exhaustive match: missing bool patterns",
                            )
                            .with_label(Label::primary(m.span, "match is not exhaustive"))
                            .with_help("add `true` and `false` patterns, or use a wildcard `_`"),
                        );
                    }
                }
                Type::I64 => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E1206,
                            "non-exhaustive match on `i64`: add a wildcard arm `_ => ...`",
                        )
                        .with_label(Label::primary(m.span, "match is not exhaustive")),
                    );
                }
                Type::Named(enum_name) => {
                    if let Some(enum_info) = self.symbols.lookup_enum(enum_name).cloned() {
                        for variant in &enum_info.variants {
                            if !covered_variants.contains(&variant.name) {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E1202,
                                        format!(
                                            "non-exhaustive match: variant `{}::{}` not covered",
                                            enum_name, variant.name
                                        ),
                                    )
                                    .with_label(Label::primary(m.span, "match is not exhaustive")),
                                );
                            }
                        }
                    }
                }
                // Generic enum: check all variants are covered.
                // Uses registered enum info so any stdlib or user generic enum is handled.
                Type::Generic(enum_name, _) => {
                    if let Some(enum_info) = self.symbols.lookup_enum(enum_name).cloned() {
                        for variant in &enum_info.variants {
                            if !covered_variants.contains(&variant.name) {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E1202,
                                        format!(
                                            "non-exhaustive match: variant `{}::{}` not covered",
                                            enum_name, variant.name
                                        ),
                                    )
                                    .with_label(Label::primary(m.span, "match is not exhaustive"))
                                    .with_help("add the missing variant or use a wildcard `_` arm"),
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        result_type.unwrap_or(Type::Void)
    }

    pub(super) fn check_match_body(&mut self, body: &MatchBody) -> Type {
        match body {
            MatchBody::Expr(expr) => self.check_expr(expr),
            MatchBody::Block(block) => {
                self.check_block(block);
                Type::Void
            }
        }
    }

    pub(super) fn check_select(&mut self, s: &SelectExpr) {
        let mut default_count = 0;
        for case in &s.cases {
            self.symbols.push_scope();
            match &case.kind {
                SelectCaseKind::Recv { binding, channel } => {
                    let ch_ty = self.check_expr(channel);
                    let elem = self.select_channel_elem(&ch_ty, channel.span());
                    if binding != "_" {
                        // Record the binding type keyed by the case span so the
                        // cooperative async lowering can frame-back it (willow-7aj),
                        // mirroring how `let`/`for` locals are recorded.
                        self.async_local_types.insert(case.span, elem.clone());
                        self.symbols.define_var(
                            binding.clone(),
                            VarInfo {
                                ty: elem,
                                mutable: false,
                                is_param: false,
                                declaration_span: case.span,
                            },
                        );
                    }
                }
                SelectCaseKind::Send { channel, value } => {
                    let ch_ty = self.check_expr(channel);
                    let elem = self.select_channel_elem(&ch_ty, channel.span());
                    let v_ty = self.check_expr(value);
                    if elem != Type::Void && v_ty != elem {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "select send value type `{}` does not match channel element `{}`",
                                    type_name(&v_ty),
                                    type_name(&elem)
                                ),
                            )
                            .with_label(Label::primary(value.span(), "wrong value type")),
                        );
                    }
                }
                SelectCaseKind::Default => default_count += 1,
            }
            self.check_block(&case.body);
            self.symbols.pop_scope();
        }
        if default_count > 1 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0807,
                    "select may have at most one `default` case",
                )
                .with_label(Label::primary(s.span, "multiple `default` cases")),
            );
        }
    }

    /// Type-check method calls on `Option<T>` and `Result<T,E>`.
    /// Returns `Some(return_type)` if the call was handled, `None` to fall through.
    pub(super) fn check_option_result_method_call(
        &mut self,
        obj_ty: &Type,
        call: &MethodCallExpr,
    ) -> Option<Type> {
        match obj_ty {
            Type::Generic(name, args) if name == "Option" => {
                let inner = args.first().cloned().unwrap_or(Type::Void);
                match call.method.as_str() {
                    "is_some" | "is_none" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!("`Option::{}` takes no arguments", call.method),
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(Type::Bool)
                    }
                    "unwrap" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    "`Option::unwrap` takes no arguments",
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(inner)
                    }
                    "expect" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::expect` expects 1 argument (message), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                        } else {
                            let msg_ty = self.check_expr(&call.args[0].expr);
                            if msg_ty != Type::String {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "expect message must be `String`, found `{}`",
                                            type_name(&msg_ty)
                                        ),
                                    )
                                    .with_label(
                                        Label::primary(
                                            call.args[0].expr.span(),
                                            "expected `String`",
                                        ),
                                    ),
                                );
                            }
                        }
                        Some(inner)
                    }
                    "unwrap_or" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::unwrap_or` expects 1 argument, got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                        } else {
                            let default_ty = self.check_expr(&call.args[0].expr);
                            if !self.types_compatible(&inner, &default_ty) {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "mismatched types: expected `{}`, found `{}`",
                                            type_name(&inner),
                                            type_name(&default_ty)
                                        ),
                                    )
                                    .with_label(
                                        Label::primary(
                                            call.args[0].expr.span(),
                                            format!("expected `{}`", type_name(&inner)),
                                        ),
                                    ),
                                );
                            }
                        }
                        Some(inner)
                    }
                    "map" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::map` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&inner, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Option::map` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&inner), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(Type::Generic("Option".to_string(), vec![*ret.clone()]))
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Option::map` expects a function `fn({}) -> U`, found `{}`",
                                            type_name(&inner), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "and_then" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::and_then` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&inner, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Option::and_then` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&inner), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(*ret.clone())
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Option::and_then` expects a function `fn({}) -> Option<U>`, found `{}`",
                                            type_name(&inner), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "or_else" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::or_else` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.is_empty() => {
                                    Some(*ret.clone())
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Option::or_else` expects a function `fn() -> Option<{}>`, found `{}`",
                                            type_name(&inner), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    _ => None,
                }
            }
            Type::Generic(name, args) if name == "Result" => {
                let ok_ty = args.first().cloned().unwrap_or(Type::Void);
                let err_ty = args.get(1).cloned().unwrap_or(Type::Void);
                match call.method.as_str() {
                    "is_ok" | "is_err" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!("`Result::{}` takes no arguments", call.method),
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(Type::Bool)
                    }
                    "unwrap" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    "`Result::unwrap` takes no arguments",
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(ok_ty)
                    }
                    "expect" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::expect` expects 1 argument (message), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                        } else {
                            let msg_ty = self.check_expr(&call.args[0].expr);
                            if msg_ty != Type::String {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "expect message must be `String`, found `{}`",
                                            type_name(&msg_ty)
                                        ),
                                    )
                                    .with_label(
                                        Label::primary(
                                            call.args[0].expr.span(),
                                            "expected `String`",
                                        ),
                                    ),
                                );
                            }
                        }
                        Some(ok_ty)
                    }
                    "unwrap_or" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::unwrap_or` expects 1 argument, got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                        } else {
                            let default_ty = self.check_expr(&call.args[0].expr);
                            if !self.types_compatible(&ok_ty, &default_ty) {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "mismatched types: expected `{}`, found `{}`",
                                            type_name(&ok_ty),
                                            type_name(&default_ty)
                                        ),
                                    )
                                    .with_label(
                                        Label::primary(
                                            call.args[0].expr.span(),
                                            format!("expected `{}`", type_name(&ok_ty)),
                                        ),
                                    ),
                                );
                            }
                        }
                        Some(ok_ty)
                    }
                    "unwrap_err" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    "`Result::unwrap_err` takes no arguments",
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(err_ty)
                    }
                    "map" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::map` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&ok_ty, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Result::map` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&ok_ty), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(Type::Generic(
                                        "Result".to_string(),
                                        vec![*ret.clone(), err_ty],
                                    ))
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Result::map` expects a function `fn({}) -> U`, found `{}`",
                                            type_name(&ok_ty), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "map_err" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::map_err` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&err_ty, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Result::map_err` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&err_ty), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(Type::Generic(
                                        "Result".to_string(),
                                        vec![ok_ty, *ret.clone()],
                                    ))
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Result::map_err` expects a function `fn({}) -> F`, found `{}`",
                                            type_name(&err_ty), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "and_then" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::and_then` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&ok_ty, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Result::and_then` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&ok_ty), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(*ret.clone())
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Result::and_then` expects a function `fn({}) -> Result<U, E>`, found `{}`",
                                            type_name(&ok_ty), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "or_else" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::or_else` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&err_ty, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Result::or_else` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&err_ty), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(*ret.clone())
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Result::or_else` expects a function `fn({}) -> Result<T, F>`, found `{}`",
                                            type_name(&err_ty), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Type-check a method on `Mutex<T>` (`get`/`set`) or `RwLock<T>`
    /// (`read`/`write`) (willow-dgwo.3).
    pub(super) fn check_lock_method_call(
        &mut self,
        lock: &str,
        elem: &Type,
        call: &MethodCallExpr,
    ) -> Type {
        // (expected arg type, return type) per method.
        let sig: Option<(Option<Type>, Type)> = match (lock, call.method.as_str()) {
            ("Mutex", "get") | ("RwLock", "read") => Some((None, elem.clone())),
            ("Mutex", "set") | ("RwLock", "write") => Some((Some(elem.clone()), Type::Void)),
            _ => None,
        };
        let Some((arg_ty, ret)) = sig else {
            for arg in &call.args {
                self.check_expr(&arg.expr);
            }
            let methods = if lock == "Mutex" {
                "get/set"
            } else {
                "read/write"
            };
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0806,
                    format!("`{lock}<T>` has no method `{}`", call.method),
                )
                .with_label(Label::primary(call.span, "unknown lock method"))
                .with_help(format!("`{lock}` supports {methods}")),
            );
            return Type::Void;
        };
        let expected_argc = usize::from(arg_ty.is_some());
        if call.args.len() != expected_argc {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "`{lock}::{}` expects {expected_argc} argument(s), got {}",
                        call.method,
                        call.args.len()
                    ),
                )
                .with_label(Label::primary(call.span, "wrong number of arguments")),
            );
        }
        if let (Some(expected), Some(arg)) = (arg_ty, call.args.first()) {
            let got = self.check_expr(&arg.expr);
            if got != expected && got != Type::Never {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "`{lock}::{}` expects `{}`, got `{}`",
                            call.method,
                            type_name(&expected),
                            type_name(&got)
                        ),
                    )
                    .with_label(Label::primary(arg.expr.span(), "wrong argument type")),
                );
            }
        }
        ret
    }

    /// Type-check a method on `AtomicI64` / `AtomicBool` (willow-dgwo.3).
    /// `load() -> T`, `store(T)`, `add(T)/sub(T) -> T` (i64 only), `swap(T) -> T`.
    pub(super) fn check_atomic_method_call(&mut self, atomic: &str, call: &MethodCallExpr) -> Type {
        let elem = if atomic == "AtomicI64" {
            Type::I64
        } else {
            Type::Bool
        };
        // (arg count, expected arg type, return type)
        let sig: Option<(usize, Option<Type>, Type)> = match call.method.as_str() {
            "load" => Some((0, None, elem.clone())),
            "store" => Some((1, Some(elem.clone()), Type::Void)),
            "swap" => Some((1, Some(elem.clone()), elem.clone())),
            "add" | "sub" if atomic == "AtomicI64" => Some((1, Some(Type::I64), Type::I64)),
            _ => None,
        };
        let Some((argc, arg_ty, ret)) = sig else {
            for arg in &call.args {
                self.check_expr(&arg.expr);
            }
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0806,
                    format!("`{atomic}` has no method `{}`", call.method),
                )
                .with_label(Label::primary(call.span, "unknown atomic method"))
                .with_help("`AtomicI64`: load/store/add/sub/swap; `AtomicBool`: load/store/swap"),
            );
            return Type::Void;
        };
        if call.args.len() != argc {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "`{atomic}::{}` expects {argc} argument(s), got {}",
                        call.method,
                        call.args.len()
                    ),
                )
                .with_label(Label::primary(call.span, "wrong number of arguments")),
            );
        }
        if let (Some(expected), Some(arg)) = (arg_ty, call.args.first()) {
            let got = self.check_expr(&arg.expr);
            if got != expected && got != Type::Never {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "`{atomic}::{}` expects `{}`, got `{}`",
                            call.method,
                            type_name(&expected),
                            type_name(&got)
                        ),
                    )
                    .with_label(Label::primary(arg.expr.span(), "wrong argument type")),
                );
            }
        }
        ret
    }

    pub(super) fn check_concurrency_method_call(
        &mut self,
        obj_ty: &Type,
        call: &MethodCallExpr,
    ) -> Option<Type> {
        // Atomic primitives (willow-dgwo.3).
        if let Type::Named(n) = obj_ty {
            if n == "AtomicI64" || n == "AtomicBool" {
                return Some(self.check_atomic_method_call(n, call));
            }
        }
        // Lock primitives (willow-dgwo.3): Mutex<T>.get/set, RwLock<T>.read/write.
        if let Type::Generic(n, args) = obj_ty {
            if (n == "Mutex" || n == "RwLock") && args.len() == 1 {
                return Some(self.check_lock_method_call(n, &args[0].clone(), call));
            }
        }
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
                    // Both `JoinHandle<T>` (spawn) and `Task<T>` (an async fn
                    // call result) are joinable: an async call schedules an
                    // eager task, so `.join()` waits for it (willow-h2vf).
                    Type::Generic(name, args)
                        if (name == "JoinHandle" || name == "Task") && args.len() == 1 =>
                    {
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
                            .with_label(Label::primary(call.span, "expected a task")),
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
                // A sent value crosses task/worker boundaries, so the channel
                // item type must be Send (willow-dgwo.6, spec §10; gated like the
                // other data-race checks).
                if self.enforce_send_sync && element_ty != Type::Void && !self.is_send(&element_ty)
                {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E2403,
                            format!(
                                "channel item type `{}` must be `Send`",
                                type_name(&element_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            call.span,
                            "this value crosses a task boundary",
                        ))
                        .with_help("send Send values (scalars, String, frozen/Sync types) through channels"),
                    );
                }
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
                    if matches!(arg.mode, CallArgMode::Reference { .. }) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1703,
                                "unexpected reference argument",
                            )
                            .with_label(Label::primary(
                                arg.span,
                                format!(
                                    "send expects `{}`, not `& {}`",
                                    type_name(&element_ty),
                                    type_name(&arg_ty)
                                ),
                            )),
                        );
                    } else if !self.types_compatible(&element_ty, &arg_ty) {
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

    pub(super) fn check_call_argument_count(
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

    pub(super) fn check_value_call_args(&mut self, params: &[Type], args: &[CallArg]) {
        let param_infos = value_param_infos(params);
        self.check_call_args_against_param_infos(&param_infos, args);
    }

    pub(super) fn check_call_args_against_param_infos(
        &mut self,
        params: &[ParamInfo],
        args: &[CallArg],
    ) {
        for (param, arg) in params.iter().zip(args) {
            self.check_call_arg_against_param(param, arg);
        }
        self.check_mut_reference_aliases(params, args);
    }

    pub(super) fn check_call_arg_against_param(&mut self, param: &ParamInfo, arg: &CallArg) {
        match (&param.mode, &arg.mode) {
            (ParamMode::Value, CallArgMode::Value) => {
                self.check_value_arg_type(&param.ty, arg);
            }
            (ParamMode::Value, CallArgMode::Reference { .. }) => {
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
                            "parameter expects `{}`, not `& {}`",
                            type_name(&param.ty),
                            type_name(&arg_ty)
                        ),
                    )),
                );
            }
            (ParamMode::Reference { .. }, CallArgMode::Value) => {
                self.check_expr(&arg.expr);
                let expr_span = arg.expr.span();
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1702,
                    "expected reference argument for reference parameter",
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
            (ParamMode::Reference { mutable, .. }, CallArgMode::Reference { .. }) => {
                self.check_reference_argument(param, arg, *mutable);
            }
        }
    }

    pub(super) fn check_value_arg_type(&mut self, param_ty: &Type, arg: &CallArg) {
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

    pub(super) fn check_reference_argument(
        &mut self,
        param: &ParamInfo,
        arg: &CallArg,
        require_mutable: bool,
    ) {
        let Some(place) = self.reference_place_info(&arg.expr, arg.span) else {
            return;
        };

        if require_mutable && !place.mutable {
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E1701,
                format!("cannot pass immutable variable `{}` as `&mut`", place.name),
            )
            .with_label(Label::primary(
                arg.span,
                "cannot pass immutable variable by mutable reference",
            ))
            .with_label(Label::secondary(
                place.declaration_span,
                "declared immutable here",
            ))
            .with_help("declare the variable as mutable");

            if !place.is_param {
                let decl = place.declaration_span;
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

        if place.ty != param.ty {
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E1705,
                "reference argument type mismatch",
            )
            .with_label(Label::primary(
                arg.span,
                format!("found `{}`", type_name(&place.ty)),
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

    pub(super) fn check_mut_reference_aliases(&mut self, params: &[ParamInfo], args: &[CallArg]) {
        let mut seen_mut_refs: Vec<(String, Span)> = Vec::new();
        let mut seen_other_uses: Vec<(String, Span)> = Vec::new();

        for (param, arg) in params.iter().zip(args) {
            let Some(name) = reference_place_key(&arg.expr) else {
                continue;
            };
            let is_mut_reference = matches!(
                (&param.mode, &arg.mode),
                (
                    ParamMode::Reference { mutable: true, .. },
                    CallArgMode::Reference { .. }
                )
            );

            if is_mut_reference {
                for (previous_name, previous_span) in &seen_mut_refs {
                    if previous_name == &name {
                        self.push_mut_reference_alias_diagnostic(
                            &name,
                            arg.span,
                            *previous_span,
                            "same mutable place passed here",
                        );
                    }
                }
                for (previous_name, previous_span) in &seen_other_uses {
                    if previous_name == &name {
                        self.push_mut_reference_alias_diagnostic(
                            &name,
                            arg.span,
                            *previous_span,
                            "same place used by another argument",
                        );
                    }
                }
                seen_mut_refs.push((name, arg.span));
            } else {
                for (previous_name, previous_span) in &seen_mut_refs {
                    if previous_name == &name {
                        self.push_mut_reference_alias_diagnostic(
                            &name,
                            arg.span,
                            *previous_span,
                            "mutable reference passed here",
                        );
                    }
                }
                seen_other_uses.push((name, arg.span));
            }
        }
    }

    pub(super) fn check_format_call(&mut self, c: &CallExpr) -> Type {
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

    pub(super) fn check_object_literal(&mut self, literal: &ObjectLiteralExpr) -> Type {
        for field in &literal.fields {
            self.check_expr(&field.value);
        }
        let mut diagnostic = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0847,
            format!(
                "object literal construction for `{}` is no longer supported",
                literal.class
            ),
        )
        .with_label(Label::primary(literal.span, "object literal used here"))
        .with_help(format!(
            "use `new {}(...)` and pass fields in constructor order",
            literal.class
        ));
        if let Some(field) = literal.fields.first() {
            diagnostic = diagnostic.with_label(Label::secondary(
                field.span,
                "named field syntax is part of the old construction form",
            ));
        }
        self.push(diagnostic);
        Type::Named(literal.class.clone())
    }

    pub(super) fn check_binary(&mut self, b: &BinaryExpr) -> Type {
        let lty = self.check_expr(&b.lhs);
        let rty = self.check_expr(&b.rhs);

        match &b.op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                if b.op == BinOp::Add && lty == Type::String && rty == Type::String {
                    return Type::String;
                }

                // String concatenation is strongly typed: `String + non-String`
                // (or the reverse) is rejected with a `toString()` suggestion
                // rather than an implicit stringify (willow-fvfc).
                if b.op == BinOp::Add && (lty == Type::String || rty == Type::String) {
                    let (non_str, side) = if lty == Type::String {
                        (&rty, "right")
                    } else {
                        (&lty, "left")
                    };
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!(
                                "cannot concatenate `String` with `{}`",
                                type_name(non_str)
                            ),
                        )
                        .with_label(Label::primary(
                            b.span,
                            format!("the {side} operand is `{}`, not `String`", type_name(non_str)),
                        ))
                        .with_help(
                            "convert explicitly with `.toString()`, e.g. `\"x = \" + value.toString()`",
                        ),
                    );
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

    pub(super) fn check_unary(&mut self, u: &UnaryExpr) -> Type {
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

    /// Type-check `ClassName::property = value` (willow-qsqf §5/§13.4): the
    /// property must be `static mut`, the value must match its type, and
    /// visibility must allow the write.
    pub(super) fn check_static_field_assign(&mut self, s: &StaticFieldAssignStmt) {
        let val_ty = self.check_expr(&s.value);
        let Some(resolved) = self.resolve_static_call_class_name(&s.class, s.span) else {
            return;
        };
        let Some((owner, info)) = self.lookup_static_prop_in_hierarchy(&resolved, &s.field) else {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0502,
                    format!("no static property `{}::{}`", resolved, s.field),
                )
                .with_label(Label::primary(s.span, "static property not found")),
            );
            return;
        };
        if !info.is_mut {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0832,
                    format!(
                        "cannot assign to immutable static property `{}::{}`",
                        owner, s.field
                    ),
                )
                .with_label(Label::primary(s.span, "cannot assign to immutable static"))
                .with_help("declare it as `static mut` if shared mutation is intended"),
            );
            return;
        }
        // Visibility: a private/protected static can only be written from inside.
        if !info.public {
            let allowed = if info.protected {
                self.can_access_protected_member(&owner)
            } else {
                self.can_access_private_member(&owner)
            };
            if !allowed {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0419,
                        format!("static property `{}::{}` is private", owner, s.field),
                    )
                    .with_label(Label::primary(s.span, "private static property")),
                );
            }
        }
        if info.ty != Type::Void && !self.types_compatible(&info.ty, &val_ty) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    self.type_mismatch_error_code(&info.ty, &val_ty),
                    format!(
                        "mismatched types: expected `{}`, found `{}`",
                        type_name(&info.ty),
                        type_name(&val_ty)
                    ),
                )
                .with_label(Label::primary(
                    s.span,
                    format!("expected `{}`", type_name(&info.ty)),
                )),
            );
        }
    }

    pub(super) fn check_block_with_narrowing(
        &mut self,
        block: &Block,
        narrowing: &NilCheckNarrowing,
    ) {
        self.narrowed_vars.push(HashMap::new());
        self.add_narrowing_to_current_scope(narrowing);
        self.check_block(block);
        self.narrowed_vars.pop();
    }

    pub(super) fn check_nil_comparison(&mut self, lty: &Type, rty: &Type, span: Span) {
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

    /// Reject a module-qualified reference to a non-`pub` type (class, interface,
    /// or enum) from another module (willow-7ihl). A module-qualified name
    /// contains `::`; same-module references are unqualified and never checked.
    pub(super) fn check_type_visibility(&mut self, name: &str, span: Span) {
        if !name.contains("::") {
            return;
        }
        let (is_private, kind) = if let Some(c) = self.symbols.lookup_class(name) {
            (!c.public, "class")
        } else if let Some(i) = self.symbols.lookup_interface(name) {
            (!i.public, "interface")
        } else if let Some(e) = self.symbols.lookup_enum(name) {
            (!e.public, "enum")
        } else {
            return;
        };
        if is_private {
            let simple = name.rsplit("::").next().unwrap_or(name);
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0419,
                    format!("{kind} `{name}` is private to its module"),
                )
                .with_label(Label::primary(
                    span,
                    "private type accessed from another module",
                ))
                .with_help(format!(
                    "mark it `pub {kind} {simple}` to use it outside its module"
                )),
            );
        }
    }
}

/// Test helper: lex+parse+type-check `source`, returning its diagnostics.
#[cfg(test)]
pub(super) fn check_source(source: &str) -> Vec<Diagnostic> {
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
