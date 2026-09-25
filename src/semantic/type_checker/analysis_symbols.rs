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
        last: bool,
    ) {
        if self.capture_call_sites {
            self.analysis_symbols.reference(
                span,
                name,
                Declaration::new(name, kind, declaration, Some(ty)),
                "member",
                last,
            );
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
            self.analysis_symbols.reference(
                span,
                name,
                Declaration::new(name, kind, declaration, None),
                "member",
                true,
            );
        }
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
