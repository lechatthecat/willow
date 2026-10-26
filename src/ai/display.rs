//! Source-like type spellings for query results. The `ty` wire tree stays the
//! compiler's exact value; `type_display` is a readable, canonical projection.
use super::*;

/// Canonical names: `$pkg<hash>::m::T` of the root package becomes `m::T`,
/// a dependency's becomes `<package>::m::T`, and a bare name that the context
/// module declares or item-imports as a type becomes the declaration's canonical
/// name. Anything else (builtins, type parameters, unknown namespaces) keeps its
/// compiler spelling.
pub(super) struct TypeNames {
    packages: HashMap<String, Option<String>>,
    /// (context module, spelling) -> canonical declaration name.
    local: HashMap<(String, String), String>,
}

impl TypeNames {
    pub(super) fn new(snapshot: &Snapshot) -> Self {
        let mut packages = HashMap::new();
        for (i, module) in snapshot.semantic.modules.iter().enumerate() {
            let package = &module.identity.package;
            // The entry module (index 0) belongs to the root package.
            let name = (i != 0).then(|| package.name.clone());
            packages
                .entry(crate::semantic::ids::package_namespace(package))
                .or_insert(name);
        }
        let root = snapshot
            .semantic
            .modules
            .first()
            .map(|m| &m.identity.package);
        let canonical = |identity: &SymbolIdentity| {
            if Some(&identity.package) == root {
                format!("{}::{}", identity.module, identity.symbol)
            } else {
                let package = &identity.package.name;
                format!("{package}::{}::{}", identity.module, identity.symbol)
            }
        };
        let is_type =
            |s: &symbols::Symbol| matches!(s.kind.as_str(), "class" | "enum" | "interface");
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
            } else if s.kind == "import" {
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
        Self { packages, local }
    }

    pub(super) fn name(&self, name: &str, module: Option<&str>) -> String {
        if let Some(path) = name.strip_prefix("$local::") {
            return path.to_owned();
        }
        if let Some(rest) = name.strip_prefix("$pkg")
            && let Some((hash, path)) = rest.split_once("::")
        {
            return match self.packages.get(&format!("$pkg{hash}")) {
                Some(Some(package)) => format!("{package}::{path}"),
                Some(None) => path.to_owned(),
                None => name.to_owned(),
            };
        }
        match module {
            Some(module) if !name.contains("::") => self
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
                Work::Type(ty) => ty,
            };
            #[cfg(test)]
            TYPE_VISITS.with(|count| count.set(count.get() + 1));
            match ty {
                Type::I64 => out.push_str("i64"),
                Type::F64 => out.push_str("f64"),
                Type::Bool => out.push_str("bool"),
                Type::String => out.push_str("String"),
                Type::Void => out.push_str("void"),
                Type::Never => out.push('!'),
                Type::Named(name) => out.push_str(&self.name(name, module)),
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
                    out.push_str(if matches!(ty, Type::Fn(..)) {
                        "fn("
                    } else {
                        "closure("
                    });
                    work.extend([Work::Type(result), Work::Text(") -> ")]);
                    list(&mut work, args);
                }
            }
        }
        out
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
            if matches!(ty, Type::Fn(..)) {
                display.insert_str(2, &binder);
            } else if matches!(ty, Type::Named(_))
                && matches!(symbol.kind.as_str(), "class" | "interface" | "enum")
            {
                display.push_str(&binder);
            }
        }
        display
    }

    /// Serialize a declaration with its readable type beside the exact `ty`.
    pub(super) fn symbol(&self, symbol: &symbols::Symbol) -> serde_json::Value {
        let mut value = serde_json::to_value(symbol).expect("symbol serializes");
        if let Some(ty) = &symbol.ty {
            value["type_display"] = self.declaration(symbol, ty).into();
        }
        self.payload(symbol, &mut value);
        value
    }

    /// A payload variant's value type is its enum; its payload types travel
    /// beside it as exact `payload` and readable `payload_display` lists.
    pub(super) fn payload(&self, symbol: &symbols::Symbol, value: &mut serde_json::Value) {
        let payload = &symbol.details.payload;
        if payload.is_empty() {
            return;
        }
        let module = symbol.identity.as_ref().map(|i| i.module.as_str());
        value["payload"] = serde_json::to_value(payload).expect("types serialize");
        value["payload_display"] = payload
            .iter()
            .map(|ty| self.render(ty, module))
            .collect::<Vec<_>>()
            .into();
    }
}

#[cfg(test)]
thread_local! { static TYPE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> TypeNames {
        TypeNames {
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
                value["type_display"],
                format!("fn<{}>({}) -> i64", params.join(", "), params.join(", "))
            );
            assert_eq!(value["ty"], serde_json::to_value(&ty).unwrap());
            assert_eq!(symbol.ty.as_ref(), Some(&ty));
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
                "closure((i64, bool)) -> (i64, bool)",
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
