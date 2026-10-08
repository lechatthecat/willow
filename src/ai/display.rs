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
        for s in &snapshot.semantic.symbols {
            let Some(identity) = &s.identity else {
                continue;
            };
            if is_type(s) {
                local.insert(
                    (identity.module.clone(), s.name.clone()),
                    canonical(identity),
                );
            } else if s.kind == "import"
                && let Some(l) = &s.location
            {
                imports.insert((&l.path, l.start, l.end), (&identity.module, &s.name));
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
        for r in &snapshot.semantic.references {
            let l = &r.location;
            if let Some(&(module, name)) = imports.get(&(&l.path, l.start, l.end))
                && let Some(target) = targets.get(r.target.as_str())
                && let Some(identity) = &target.identity
            {
                local.insert((module.clone(), name.clone()), canonical(identity));
            }
        }
        Self { packages, local }
    }

    pub(super) fn name(&self, name: &str, module: Option<&str>) -> String {
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

    /// Serialize a declaration with its readable type beside the exact `ty`.
    pub(super) fn symbol(&self, symbol: &symbols::Symbol) -> serde_json::Value {
        let mut value = serde_json::to_value(symbol).expect("symbol serializes");
        if let Some(ty) = &symbol.ty {
            let module = symbol.identity.as_ref().map(|i| i.module.as_str());
            value["type_display"] = self.render(ty, module).into();
        }
        value
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
