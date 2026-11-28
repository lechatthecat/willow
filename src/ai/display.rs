//! Source-like type spellings for query results. The `ty` wire tree stays the
//! compiler's exact value; `type_display` is a readable, canonical projection.
use super::*;
use crate::ai::SymbolKind;
use crate::qualified_name::QualifiedName;

/// Canonical names: `$pkg<hash>::m::T` of the root package becomes `m::T`,
/// a dependency's becomes `<package>::m::T`, and a bare name that the context
/// module declares or item-imports as a type becomes the declaration's canonical
/// name. Anything else (builtins, type parameters, unknown namespaces) keeps its
/// compiler spelling.
pub(super) struct TypeNames {
    packages: HashMap<String, Option<String>>,
    namespaces: crate::qualified_name::NameMetadata,
    /// (context module, spelling) -> canonical declaration name.
    local: HashMap<(String, String), String>,
}

fn package_names(snapshot: &Snapshot) -> HashMap<String, Option<String>> {
    let mut packages = HashMap::new();
    for (i, module) in snapshot.semantic.modules.iter().enumerate() {
        let package = &module.identity.package;
        // The entry module (index 0) belongs to the root package.
        let name = (i != 0).then(|| package.name.clone());
        packages
            .entry(crate::semantic::ids::package_namespace(package))
            .or_insert(name);
    }
    packages
}

/// Render restored source types after package attachment, using each witness's
/// origin (not the querying caller). Consume live-only types before storage.
pub(super) fn present_witness_types(snapshot: &mut Snapshot, names: &TypeNames) {
    let modules: HashMap<_, _> = snapshot
        .semantic
        .modules
        .iter()
        .map(|module| (module.path.as_str(), module.identity.module.as_str()))
        .collect();
    for fact in snapshot.semantic.witnesses.values_mut().flatten() {
        let witness = &mut fact.witness;
        let Some(awaited) = witness.awaited.take() else {
            continue;
        };
        let module = witness
            .location()
            .and_then(|location| modules.get(location.path.as_str()).copied());
        let operand = awaited.operand.as_ref().map(|ty| names.awaited(ty, module));
        let operand_name = operand.as_deref().unwrap_or("an unresolved operand type");
        witness.reason = Some(if awaited.parks {
            format!(
                "In an async function, await of {operand_name} lowers to a scheduler suspension: the task parks until the awaited operation completes; no OS thread blocks."
            )
        } else {
            format!(
                "Conservative await bound for {operand_name}; waiting may park or block depending on the selected lowering. This summary does not establish which path executes."
            )
        });
        witness.awaited_type = Some(operand);
    }
}

impl TypeNames {
    pub(super) fn new(snapshot: &Snapshot) -> Self {
        let packages = package_names(snapshot);
        let root = snapshot
            .semantic
            .modules
            .first()
            .map(|m| &m.identity.package);
        let canonical = |identity: &SymbolIdentity| {
            if Some(&identity.package) == root {
                QualifiedName::join(&identity.module, &identity.symbol)
            } else {
                let package = &identity.package.name;
                QualifiedName::from_segments([package.as_str(), &identity.module, &identity.symbol])
            }
        };
        let is_type = |s: &symbols::Symbol| {
            matches!(
                s.kind,
                SymbolKind::Class | SymbolKind::Enum | SymbolKind::Interface
            )
        };
        let mut local = HashMap::new();
        let mut imports = HashMap::new();
        let mut import_ids = HashMap::new();
        for s in &snapshot.semantic.symbols {
            let Some(identity) = &s.identity else {
                continue;
            };
            if is_type(s) {
                local.insert(
                    (identity.module.clone(), s.name.clone()),
                    canonical(identity),
                );
            } else if s.kind == SymbolKind::Import {
                if let Some(l) = &s.location {
                    imports.insert((&l.path, l.start, l.end), (&identity.module, &s.name));
                }
                import_ids.insert(s.id.as_str(), (&identity.module, &s.name));
            }
        }
        // An item import's token resolves to its target declaration.
        let targets: HashMap<&str, &symbols::Symbol> = snapshot
            .semantic
            .symbols
            .iter()
            .filter(|s| is_type(s))
            .map(|s| (s.id.as_str(), s))
            .collect();
        // An aliased item import (`import m::T as U`) declares `U` on the alias
        // token while its target is referenced on `T`; a use of `U` carries both
        // the import and the type reference on one token.
        let mut used_imports = Vec::new();
        let mut used_types = HashMap::new();
        for r in &snapshot.semantic.references {
            let l = &r.location;
            let key = (&l.path, l.start, l.end);
            if let Some(target) = targets.get(r.target.as_str())
                && let Some(identity) = &target.identity
            {
                if let Some(&(module, name)) = imports.get(&key) {
                    local.insert((module.clone(), name.clone()), canonical(identity));
                }
                used_types.insert(key, identity);
            } else if let Some(&import) = import_ids.get(r.target.as_str()) {
                used_imports.push((key, import));
            }
        }
        for (key, (module, name)) in used_imports {
            if let Some(identity) = used_types.get(&key) {
                local
                    .entry((module.clone(), name.clone()))
                    .or_insert_with(|| canonical(identity));
            }
        }
        let mut namespaces = crate::qualified_name::NameMetadata::default();
        namespaces.register(
            crate::semantic::ids::LOCAL_NAMESPACE,
            "",
            crate::qualified_name::SyntheticKind::LocalNamespace,
        );
        Self {
            packages,
            local,
            namespaces,
        }
    }

    pub(super) fn name(&self, name: &str, module: Option<&str>) -> String {
        if let Some((namespace, path)) = QualifiedName::new(name).split_first() {
            if self.namespaces.kind(namespace)
                == crate::qualified_name::SyntheticKind::LocalNamespace
            {
                return path.to_owned();
            }
            if let Some(package) = self.packages.get(namespace) {
                return match package {
                    Some(package) => QualifiedName::join(package, path),
                    None => path.to_owned(),
                };
            }
        }
        match module {
            Some(module) if !QualifiedName::new(&name).is_qualified() => self
                .local
                .get(&(module.to_owned(), name.to_owned()))
                .cloned()
                .unwrap_or_else(|| name.to_owned()),
            _ => name.to_owned(),
        }
    }

    /// Iterative, like the compiler's `Debug`, so deep types cannot overflow.
    pub(super) fn render(&self, ty: &Type, module: Option<&str>) -> String {
        enum Work<'a> {
            Type(&'a Type),
            Text(&'static str),
            Length(u64),
        }
        fn list<'a>(work: &mut Vec<Work<'a>>, items: &'a [Type]) {
            for (i, item) in items.iter().enumerate().rev() {
                work.push(Work::Type(item));
                if i != 0 {
                    work.push(Work::Text(", "));
                }
            }
        }
        let mut out = String::new();
        let mut work = vec![Work::Type(ty)];
        while let Some(task) = work.pop() {
            let ty = match task {
                Work::Text(text) => {
                    out.push_str(text);
                    continue;
                }
                Work::Length(n) => {
                    out.push_str(&n.to_string());
                    continue;
                }
                Work::Type(ty) => ty,
            };
            #[cfg(test)]
            TYPE_VISITS.with(|count| count.set(count.get() + 1));
            match ty {
                Type::I8 => out.push_str("i8"),
                Type::I16 => out.push_str("i16"),
                Type::I32 => out.push_str("i32"),
                Type::U16 => out.push_str("u16"),
                Type::U32 => out.push_str("u32"),
                Type::U64 => out.push_str("u64"),
                Type::Char => out.push_str("char"),
                Type::U8 => out.push_str("u8"),
                Type::I64 => out.push_str("i64"),
                Type::F64 => out.push_str("f64"),
                Type::Bool => out.push_str("bool"),
                Type::String => out.push_str("String"),
                Type::Void => out.push_str("void"),
                Type::Never => out.push('!'),
                Type::Named(name) => out.push_str(&self.name(name, module)),
                Type::FixedArray(element, n) => {
                    out.push('[');
                    work.extend([
                        Work::Text("]"),
                        Work::Length(*n),
                        Work::Text("; "),
                        Work::Type(element),
                    ]);
                }
                Type::Array(element) => {
                    out.push_str("Array<");
                    work.extend([Work::Text(">"), Work::Type(element)]);
                }
                Type::Generic(name, args) if crate::parser::tuples::is_tuple(name) => {
                    out.push('(');
                    work.push(Work::Text(if args.len() == 1 { ",)" } else { ")" }));
                    list(&mut work, args);
                }
                Type::Generic(name, args) => {
                    out.push_str(&self.name(name, module));
                    out.push('<');
                    work.push(Work::Text(">"));
                    list(&mut work, args);
                }
                Type::Fn(args, result) | Type::Closure(args, result) => {
                    out.push_str("fn(");
                    work.extend([Work::Type(result), Work::Text(") -> ")]);
                    list(&mut work, args);
                }
            }
        }
        out
    }

    /// Keep the source unit spelling for task payloads; Future<void> retains
    /// its existing spelling. All other nodes use the canonical type renderer.
    fn awaited(&self, ty: &Type, module: Option<&str>) -> String {
        match ty {
            Type::Generic(name, args)
                if matches!(args.as_slice(), [Type::Void])
                    && crate::semantic::type_checker::awaitable_task_type(ty).is_some() =>
            {
                format!("{}<()>", self.name(name, module))
            }
            _ => self.render(ty, module),
        }
    }

    /// Generic binders belong to declarations, not to callable value types.
    /// Keep instantiated receiver/call types on the ordinary render path.
    pub(super) fn declaration(&self, symbol: &symbols::Symbol, ty: &Type) -> String {
        let module = symbol.identity.as_ref().map(|i| i.module.as_str());
        let mut display = self.render(ty, module);
        let params = if symbol.details.type_params.is_empty() {
            symbol.details.owner_type_params.as_ref()
        } else {
            &symbol.details.type_params
        };
        if !params.is_empty() {
            let binder = format!("<{}>", params.join(", "));
            // A function-valued field or local is a value of the owner's
            // instantiation, not a generic declaration of its own.
            if matches!(ty, Type::Fn(..))
                && !matches!(
                    symbol.kind,
                    SymbolKind::Field
                        | SymbolKind::Binding
                        | SymbolKind::Parameter
                        | SymbolKind::Static
                        | SymbolKind::Const
                )
            {
                display.insert_str(2, &binder);
            } else if matches!(ty, Type::Named(_))
                && matches!(
                    symbol.kind,
                    SymbolKind::Class | SymbolKind::Interface | SymbolKind::Enum
                )
            {
                display.push_str(&binder);
            }
        }
        if symbol.details.is_async && matches!(ty, Type::Fn(..)) {
            display.insert_str(0, "async ");
        }
        display
    }

    /// Present a declaration without converting compiler facts to JSON.
    pub(super) fn symbol(&self, symbol: &symbols::Symbol) -> query_result::SymbolRecord {
        let mut result = query_result::SymbolRecord::from(symbol);
        result.type_display = symbol.ty.as_ref().map(|ty| self.declaration(symbol, ty));
        (result.payload, result.payload_display) = self.payload(symbol);
        result
    }

    pub(super) fn payload(&self, symbol: &symbols::Symbol) -> (Vec<Type>, Vec<String>) {
        let module = symbol.identity.as_ref().map(|i| i.module.as_str());
        (
            symbol.details.payload.clone(),
            symbol
                .details
                .payload
                .iter()
                .map(|ty| self.render(ty, module))
                .collect(),
        )
    }
}

#[cfg(test)]
thread_local! { static TYPE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn witness_type_projection_preserves_nullable_fields_and_storage() {
        use super::super::evidence::{AwaitType, EffectFact, Witness, WitnessKind};
        for count in [1, 16, 256, 1024] {
            let operand = Type::Generic(
                "Task".into(),
                vec![Type::Generic(
                    "Result".into(),
                    vec![
                        Type::Generic("$local::model::Box".into(), vec![Type::I64]),
                        Type::String,
                    ],
                )],
            );
            let mut witness = Witness::new(WitnessKind::RuntimeCapability);
            witness.awaited = Some(AwaitType {
                operand: Some(operand),
                parks: true,
            });
            let fact = EffectFact {
                effect: 1,
                witness,
                paths: None,
            };
            let mut snapshot = Snapshot {
                edit_context: None,
                version: 1,
                compiler: String::new(),
                compatibility: String::new(),
                workspace: String::new(),
                revision: String::new(),
                sources: BTreeMap::new(),
                functions: vec![],
                semantic: Default::default(),
            };
            let facts = snapshot
                .semantic
                .witnesses
                .entry("unchanged-id".into())
                .or_default();
            facts.extend(std::iter::repeat_n(fact, count));
            let mut unresolved = Witness::new(WitnessKind::RuntimeCapability);
            unresolved.awaited = Some(AwaitType {
                operand: None,
                parks: false,
            });
            facts.push(EffectFact {
                effect: 2,
                witness: unresolved,
                paths: None,
            });
            facts.push(EffectFact {
                effect: 4,
                witness: Witness::new(WitnessKind::Unavailable),
                paths: None,
            });
            TYPE_VISITS.with(|visits| visits.set(0));
            present_witness_types(&mut snapshot, &names());
            assert_eq!(TYPE_VISITS.with(|visits| visits.get()), 5 * count);
            let facts = &snapshot.semantic.witnesses["unchanged-id"];
            assert_eq!(facts.len(), count + 2);
            for fact in &facts[..count] {
                assert_eq!(
                    fact.witness.awaited_type.as_ref().unwrap().as_deref(),
                    Some("Task<Result<model::Box<i64>, String>>")
                );
                assert_eq!(
                    fact.witness.reason.as_deref(),
                    Some(
                        "In an async function, await of Task<Result<model::Box<i64>, String>> lowers to a scheduler suspension: the task parks until the awaited operation completes; no OS thread blocks."
                    )
                );
                assert_eq!(fact.effect, 1);
                assert!(fact.witness.awaited.is_none());
            }
            assert_eq!(facts[count].witness.awaited_type, Some(None));
            assert_eq!(facts[count + 1].witness.awaited_type, None);
            assert!(
                facts[count]
                    .witness
                    .reason
                    .as_deref()
                    .unwrap()
                    .starts_with("Conservative await bound for an unresolved operand type;")
            );
            let stored = serde_json::to_vec(&snapshot).unwrap();
            let mut restored: Snapshot = serde_json::from_slice(&stored).unwrap();
            present_witness_types(&mut restored, &names());
            assert_eq!(TYPE_VISITS.with(|visits| visits.get()), 5 * count);
            assert_eq!(stored, serde_json::to_vec(&restored).unwrap());
            println!(
                "witnesses={} rendered_type_nodes={} stored_bytes={}",
                count + 2,
                count * 5,
                stored.len()
            );
        }
    }

    fn names() -> TypeNames {
        let mut namespaces = crate::qualified_name::NameMetadata::default();
        namespaces.register(
            crate::semantic::ids::LOCAL_NAMESPACE,
            "",
            crate::qualified_name::SyntheticKind::LocalNamespace,
        );
        TypeNames {
            namespaces,
            packages: HashMap::new(),
            local: HashMap::new(),
        }
    }

    #[test]
    fn generic_declaration_display_scales_with_output() {
        for n in [1, 2, 16, 128] {
            let params: Vec<_> = (0..n).map(|i| format!("T{i}")).collect();
            let ty = Type::Fn(
                params.iter().cloned().map(Type::Named).collect(),
                Box::new(Type::I64),
            );
            let symbol = symbols::Symbol {
                origin: Default::default(),
                identity: None,
                id: "generic".into(),
                name: "generic".into(),
                source_name: None,
                kind: "function".into(),
                location: None,
                details: symbols::DeclarationDetails {
                    type_params: params.clone(),
                    ..Default::default()
                },
                ty: Some(ty.clone()),
            };
            TYPE_VISITS.with(|count| count.set(0));
            let value = names().symbol(&symbol);
            assert_eq!(TYPE_VISITS.with(|count| count.get()), n + 2);
            assert_eq!(
                value.type_display,
                Some(format!(
                    "fn<{}>({}) -> i64",
                    params.join(", "),
                    params.join(", ")
                ))
            );
            assert_eq!(value.ty.as_ref(), Some(&ty));
            assert_eq!(symbol.ty.as_ref(), Some(&ty));
            let mut asynchronous = symbol.clone();
            asynchronous.details.is_async = true;
            TYPE_VISITS.with(|count| count.set(0));
            let async_value = names().symbol(&asynchronous);
            assert_eq!(TYPE_VISITS.with(|count| count.get()), n + 2);
            assert_eq!(
                async_value.type_display,
                Some(format!("async {}", value.type_display.as_deref().unwrap()))
            );
            assert_eq!(async_value.ty, value.ty);
        }
    }

    #[test]
    fn standalone_display_projects_canonical_namespace_without_changing_type() {
        let ty = Type::Generic(
            "$local::b::Box".into(),
            vec![Type::Named("$local::a::Cell".into())],
        );
        assert_eq!(names().render(&ty, None), "b::Box<a::Cell>");
        let Type::Generic(name, _) = &ty else {
            unreachable!()
        };
        assert_eq!(name, "$local::b::Box");
        assert_eq!(names().name("$localish::Keep", None), "$localish::Keep");
    }

    #[test]
    fn tuple_display_composes_with_every_type_container() {
        let pair = Type::Generic("$Tuple2".into(), vec![Type::I64, Type::Bool]);
        let cases = [
            (pair.clone(), "(i64, bool)"),
            (Type::Generic("$Tuple1".into(), vec![Type::I64]), "(i64,)"),
            (Type::Generic("$Tuple0".into(), vec![]), "()"),
            (Type::Array(Box::new(pair.clone())), "Array<(i64, bool)>"),
            (
                Type::Generic("Box".into(), vec![pair.clone()]),
                "Box<(i64, bool)>",
            ),
            (
                Type::Fn(vec![pair.clone()], Box::new(pair.clone())),
                "fn((i64, bool)) -> (i64, bool)",
            ),
            (
                Type::Closure(vec![pair.clone()], Box::new(pair)),
                "fn((i64, bool)) -> (i64, bool)",
            ),
            (
                Type::Generic("$TupleLike".into(), vec![Type::I64]),
                "$TupleLike<i64>",
            ),
            (
                Type::Generic("m::Tuple2".into(), vec![Type::I64]),
                "m::Tuple2<i64>",
            ),
            (
                Type::Generic(
                    "$Tuple4".into(),
                    vec![Type::F64, Type::String, Type::Void, Type::Never],
                ),
                "(f64, String, void, !)",
            ),
        ];
        for (ty, expected) in cases {
            assert_eq!(names().render(&ty, None), expected);
        }
    }

    #[test]
    fn tuple_display_visits_each_node_once_for_wide_and_deep_types() {
        for n in [1, 16, 256, 1024] {
            let wide = Type::Generic(format!("$Tuple{n}"), vec![Type::I64; n]);
            TYPE_VISITS.with(|count| count.set(0));
            let output = names().render(&wide, None);
            assert_eq!(TYPE_VISITS.with(|count| count.get()), n + 1);
            assert_eq!(output.matches("i64").count(), n);
            let mut deep = Type::I64;
            for _ in 0..n {
                deep = Type::Generic("$Tuple2".into(), vec![Type::Bool, deep]);
            }
            TYPE_VISITS.with(|count| count.set(0));
            let output = names().render(&deep, None);
            assert_eq!(TYPE_VISITS.with(|count| count.get()), 2 * n + 1);
            assert_eq!(output.len(), 8 * n + 3);
            println!(
                "n={n} wide_visits={} deep_visits={} deep_bytes={}",
                n + 1,
                2 * n + 1,
                output.len()
            );
        }
    }
}
