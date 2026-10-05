use super::*;
use crate::semantic::analysis_symbols::Declaration;

impl TypeChecker {
    pub(super) fn record_annotation_uses(&mut self, program: &Program) {
        if !self.capture_call_sites {
            return;
        }
        let mut owners: Vec<_> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Enum(e) => Some((
                    e.span,
                    e.type_params
                        .iter()
                        .map(String::as_str)
                        .collect::<HashSet<_>>(),
                )),
                Item::Interface(i) => Some((
                    i.span,
                    i.type_params
                        .iter()
                        .map(String::as_str)
                        .collect::<HashSet<_>>(),
                )),
                _ => None,
            })
            .collect();
        owners.sort_by_key(|(span, _)| (span.file_id.0, span.start));
        for import in &program.imports {
            let local = import
                .alias
                .as_deref()
                .unwrap_or_else(|| import.path.rsplit("::").next().unwrap());
            if self.symbols.lookup_class(local).is_some()
                || self.symbols.lookup_enum(local).is_some()
                || self.symbols.lookup_interface(local).is_some()
            {
                self.record_type_use(
                    &import.path,
                    &Type::Named(self.canonical_type_name(local)),
                    import.span,
                );
            } else if let Some(info) = self.symbols.lookup_func(local) {
                let name = import.path.rsplit("::").next().unwrap();
                self.analysis_symbols.reference(
                    import.span,
                    name,
                    Declaration::new(name, "function", info.declaration_span, None),
                    "import-target",
                    false,
                );
            }
        }
        for usage in &program.type_uses {
            let p = owners.partition_point(|(span, _)| {
                (span.file_id.0, span.start) <= (usage.span.file_id.0, usage.span.start)
            });
            if let Some((owner, params)) = p.checked_sub(1).map(|i| &owners[i])
                && owner.file_id == usage.span.file_id
                && usage.span.end <= owner.end
                && params.contains(usage.name.as_str())
            {
                self.analysis_symbols.reference(
                    usage.span,
                    &usage.name,
                    Declaration::new(&usage.name, "type-parameter", *owner, None),
                    "type",
                    false,
                );
                continue;
            }
            let canonical = self.canonical_type_name(&usage.name);
            let before = self.analysis_symbols.references.len();
            self.record_type_use(&usage.name, &Type::Named(canonical.clone()), usage.span);
            if !self.analysis_symbols.references[before..]
                .iter()
                .any(|r| r.role == "type")
            {
                let ty = match canonical.as_str() {
                    "i64" => Type::I64,
                    "f64" => Type::F64,
                    "bool" => Type::Bool,
                    "String" => Type::String,
                    "void" => Type::Void,
                    _ => Type::Named(canonical.clone()),
                };
                self.analysis_symbols.reference(
                    usage.span,
                    &usage.name,
                    Declaration::new(&canonical, "builtin-type", Span::new(0, 0, 0, 0), Some(ty)),
                    "type",
                    false,
                );
            }
        }
    }
    pub(super) fn record_variant_use(&mut self, owner: &str, name: &str, span: Span) {
        if self.capture_call_sites
            && let Some(info) = self.symbols.lookup_enum(owner)
        {
            self.analysis_symbols.member_references.push(
                crate::semantic::analysis_symbols::MemberReference {
                    owner: info.declaration_span,
                    name: name.into(),
                    span,
                    kind: "variant".into(),
                },
            );
        }
    }
    fn record_import_use(&mut self, written: &str, span: Span) {
        let first = written.split("::").next().unwrap_or(written);
        if let Some(&Some(declaration)) = self.imported_names.get(first) {
            self.analysis_symbols.reference(
                span,
                first,
                Declaration::new(first, "import", declaration, None),
                "import",
                false,
            );
        }
    }
    pub(super) fn record_binding_use(&mut self, name: &str, span: Span, role: &str) {
        if !self.capture_call_sites {
            return;
        }
        if let Some(info) = self.symbols.lookup_var(name) {
            let target = Declaration::new(
                name,
                if info.is_param {
                    "parameter"
                } else {
                    "binding"
                },
                info.declaration_span,
                None,
            );
            self.analysis_symbols
                .reference(span, name, target, role, false);
        }
    }
    pub(super) fn record_member_use(
        &mut self,
        name: &str,
        kind: &str,
        span: Span,
        declaration: Span,
        ty: Type,
        role: &str,
    ) {
        if self.capture_call_sites {
            self.analysis_symbols.reference(
                span,
                name,
                Declaration::new(name, kind, declaration, Some(ty)),
                role,
                true,
            );
        }
    }
    /// Classify only references produced while checking this storage expression.
    /// Compound lowering captures the source place in a reserved temporary: its
    /// initializer supplies both the source read and the element-write evidence.
    pub(super) fn record_element_write(
        &mut self,
        expression: &Expr,
        start: usize,
        keep_read: bool,
    ) {
        if !self.capture_call_sites {
            return;
        }
        let mut target = expression;
        while let Expr::Index(array, ..) = target {
            target = array;
        }
        if !matches!(
            target,
            Expr::Var(..) | Expr::FieldAccess(..) | Expr::StaticField(..)
        ) {
            return;
        }
        let end = self.analysis_symbols.references.len();
        for index in start..end {
            let reference = &mut self.analysis_symbols.references[index];
            if reference.span == target.span()
                && matches!(
                    reference.target.kind.as_str(),
                    "binding" | "parameter" | "field" | "static-field"
                )
            {
                if keep_read {
                    let mut write = reference.clone();
                    write.role = "write-element".into();
                    self.analysis_symbols.references.push(write);
                } else {
                    reference.role = "write-element".into();
                }
            }
        }
    }
    pub(super) fn record_method_use(
        &mut self,
        name: &str,
        kind: &str,
        mut span: Span,
        declaration: Span,
        args: &[CallArg],
    ) {
        if let Some(arg) = args.first() {
            span.end = arg.span.start;
        }
        if self.capture_call_sites {
            self.member_uses.insert(
                (span.file_id, span.start),
                self.analysis_symbols.references.len(),
            );
            self.analysis_symbols.reference(
                span,
                name,
                Declaration::new(name, kind, declaration, None),
                "member",
                true,
            );
        }
    }
    /// A successfully checked member call that recorded no source method is a
    /// builtin (`Array::len`, `AtomicI64::add`, `String::len`, ...). Record it
    /// as a proven non-source target so rename can skip same-spelled tokens.
    /// O(1): the indexed member use counts only if it was recorded after
    /// `start` (receiver chains such as `a.m().len()` share this start).
    pub(super) fn record_builtin_method_use(
        &mut self,
        call: &MethodCallExpr,
        start: usize,
        errors: usize,
    ) {
        if !self.capture_call_sites
            || self.error_generation != errors
            || self
                .member_uses
                .get(&(call.span.file_id, call.span.start))
                .is_some_and(|&i| {
                    i >= start
                        && self.analysis_symbols.references.get(i).is_some_and(|r| {
                            r.role == "member"
                                && r.span.file_id == call.span.file_id
                                && r.span.start == call.span.start
                        })
                })
        {
            return;
        }
        self.record_method_use(
            &call.method,
            "builtin-method",
            call.span,
            Span::new(0, 0, 0, 0),
            &call.args,
        );
    }
    pub(super) fn record_type_use(&mut self, written: &str, normalized: &Type, span: Span) {
        if !self.capture_call_sites {
            return;
        }
        self.record_import_use(written, span);
        let name = match normalized {
            Type::Named(n) | Type::Generic(n, _) => n.as_str(),
            _ => return,
        };
        let target = if let Some(info) = self.symbols.lookup_class(name) {
            Some(Declaration::new(
                &info.name,
                "class",
                info.declaration_span,
                None,
            ))
        } else if let Some(info) = self.symbols.lookup_interface(name) {
            Some(Declaration::new(
                &info.name,
                "interface",
                info.declaration_span,
                None,
            ))
        } else {
            self.symbols
                .lookup_enum(name)
                .map(|info| Declaration::new(&info.name, "enum", info.declaration_span, None))
        };
        let target = target.unwrap_or_else(|| {
            Declaration::new(
                name,
                "builtin-type",
                Span::new(0, 0, 0, 0),
                Some(normalized.clone()),
            )
        });
        self.analysis_symbols
            .reference(span, written, target, "type", false);
    }
    pub(super) fn record_symbol_use(&mut self, expr: &Expr) {
        if !self.capture_call_sites {
            return;
        }
        match expr {
            Expr::Var(name, span, _) => {
                self.record_binding_use(name, *span, "read");
                if self.symbols.lookup_var(name).is_none() {
                    self.record_import_use(name, *span);
                    if let Some(info) = self.symbols.lookup_func(name) {
                        let d = Declaration::new(
                            name,
                            "function",
                            info.declaration_span,
                            Some(Type::Fn(
                                info.params.clone(),
                                Box::new(info.return_type.clone()),
                            )),
                        );
                        self.analysis_symbols
                            .reference(*span, name, d, "value", false);
                    }
                }
            }
            Expr::Call(call) => {
                self.record_binding_use(&call.callee, call.span, "call");
                if self.symbols.lookup_var(&call.callee).is_none() {
                    self.record_import_use(&call.callee, call.span);
                    if let Some(info) = self.symbols.lookup_func(&call.callee) {
                        let name = call.callee.rsplit("::").next().unwrap_or(&call.callee);
                        self.analysis_symbols.reference(
                            call.span,
                            name,
                            Declaration::new(name, "function", info.declaration_span, None),
                            "call",
                            false,
                        );
                    }
                }
            }
            Expr::New(call) => {
                if let Some(ty) = self.expr_types.get(&call.id).cloned() {
                    self.record_type_use(&call.class_name, &ty, call.span);
                }
            }
            Expr::StaticCall(call) => {
                let name = self
                    .static_call_classes
                    .get(&call.id)
                    .unwrap_or(&call.class)
                    .clone();
                self.record_type_use(&call.class, &Type::Named(name), call.span);
            }
            Expr::Print(_, newline, span, _) => {
                let name = if *newline { "println" } else { "print" };
                self.analysis_symbols.reference(
                    *span,
                    name,
                    Declaration::new(name, "builtin-function", Span::new(0, 0, 0, 0), None),
                    "call",
                    false,
                );
            }
            Expr::StaticField(field) => {
                let name = self
                    .static_call_classes
                    .get(&field.id)
                    .unwrap_or(&field.class)
                    .clone();
                self.record_type_use(&field.class, &Type::Named(name), field.span);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod buildgraph_tests {
    use super::*;
    use crate::{lexer::Lexer, parser::Parser};
    #[test]
    fn compound_element_roles_scale_with_sites_and_index_depth() {
        for n in [16, 64, 256, 1024] {
            for depth in [1, 4, 16] {
                let mut source = format!(
                    "fn f() {{ let xs = {}1{}; let i = 0;",
                    "[".repeat(depth),
                    "]".repeat(depth)
                );
                source.push_str(&format!("xs{} += 1;", "[i]".repeat(depth)).repeat(n));
                source.push('}');
                let (program, parse) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                assert!(parse.is_empty());
                let mut checker = TypeChecker::new();
                checker.capture_call_sites = true;
                checker.check_program(&program);
                assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                let references = &checker.analysis_symbols.references;
                for (name, role, count) in [
                    ("xs", "read", n),
                    ("xs", "write-element", n),
                    ("i", "read", n * depth),
                ] {
                    assert_eq!(
                        references
                            .iter()
                            .filter(|r| r.written == name && r.role == role)
                            .count(),
                        count,
                        "n={n}, depth={depth}, {name}/{role}"
                    );
                }
                // Four synthetic temporary references per lowered assignment.
                assert_eq!(references.len(), n * (depth + 6));
            }
        }
    }
    #[test]
    fn element_mutation_roles_scale_with_sites() {
        for n in [16, 64, 256, 1024] {
            let mut source = String::from("fn f() { let xs = [1]; let i = 0;");
            source.push_str(&"xs[i] = xs[i] + 1;".repeat(n));
            source.push('}');
            let (program, parse) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            assert!(parse.is_empty());
            let mut checker = TypeChecker::new();
            checker.capture_call_sites = true;
            checker.check_program(&program);
            assert!(checker.errors.is_empty(), "{:?}", checker.errors);
            let references = &checker.analysis_symbols.references;
            assert_eq!(
                references
                    .iter()
                    .filter(|r| r.role == "write-element")
                    .count(),
                n
            );
            assert_eq!(
                references.iter().filter(|r| r.role == "read").count(),
                3 * n
            );
        }
    }
}
