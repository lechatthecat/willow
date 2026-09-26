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
