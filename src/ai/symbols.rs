//! Serialization of compiler-owned declaration/reference evidence.
use super::*;
use crate::semantic::analysis_symbols::{Declaration, Facts};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Symbol {
    #[serde(default)]
    pub identity: Option<SymbolIdentity>,
    pub id: String,
    pub name: String,
    pub kind: String,
    pub location: Option<Location>,
    pub ty: Option<Type>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    #[serde(default)]
    pub identity: Option<SymbolIdentity>,
    pub target: String,
    pub location: Location,
    pub role: String,
}

/// Source owners, captured before module bodies are offloaded. Inherited/default
/// method copies retain their declaration span and must not acquire a new owner.
pub(super) fn owners(program: &Program) -> Vec<(Span, String)> {
    let mut owners = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) => owners.push((f.span, f.name.clone())),
            Item::Class(c) => {
                owners.push((c.span, c.name.clone()));
                for m in &c.methods {
                    if c.span.contains(m.span) {
                        owners.push((m.span, format!("{}::{}", c.name, m.name)));
                    }
                }
                for init in &c.constructors {
                    if c.span.contains(init.span) {
                        owners.push((init.span, format!("{}::init@{}", c.name, init.span.start)));
                    }
                }
            }
            Item::Enum(e) => owners.push((e.span, e.name.clone())),
            Item::Interface(i) => {
                owners.push((i.span, i.name.clone()));
                for m in &i.methods {
                    owners.push((m.span, format!("{}::{}", i.name, m.name)));
                }
            }
        }
    }
    owners
}

pub(super) fn declarations(
    program: &Program,
    facts: &mut Facts,
    checked: &crate::semantic::symbols::SymbolTable,
) {
    // Reuse registered signatures, including normalized import identities.
    // AST spellings alone are not the checked type of a declaration.
    let mut types = HashMap::new();
    let mut signature = |span: Span, params: &[Param], resolved: &[Type], result: &Type| {
        types.insert(span, Type::Fn(resolved.to_vec(), Box::new(result.clone())));
        for (param, ty) in params.iter().zip(resolved) {
            types.insert(param.span, ty.clone());
        }
    };
    for item in &program.items {
        match item {
            Item::Function(f) => {
                if let Some(info) = checked.lookup_func(&f.name) {
                    signature(f.span, &f.params, &info.params, &info.return_type);
                }
            }
            Item::Class(c) => {
                if let Some(info) = checked.lookup_class(&c.name) {
                    for method in &c.methods {
                        if let Some(m) = info.methods.get(&method.name) {
                            signature(method.span, &method.params, &m.params, &m.return_type);
                        }
                    }
                }
            }
            Item::Interface(i) => {
                if let Some(info) = checked.lookup_interface(&i.name) {
                    for method in &i.methods {
                        if let Some(m) = info.methods.get(&method.name) {
                            signature(method.span, &method.params, &m.params, &m.return_type);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for item in &program.items {
        if let Item::Class(c) = item
            && let Some(info) = checked.lookup_class(&c.name)
        {
            for field in &c.fields {
                let ty = if field.is_static {
                    info.static_props.get(&field.name).map(|f| &f.ty)
                } else {
                    info.fields.get(&field.name).map(|f| &f.ty)
                };
                if let Some(ty) = ty {
                    types.insert(field.span, ty.clone());
                }
            }
        }
    }
    for item in &program.items {
        if let Item::Enum(e) = item
            && let Some(info) = checked.lookup_enum(&e.name)
        {
            let ty = Type::Named(info.name.clone());
            types.insert(e.span, ty.clone());
            for variant in &e.variants {
                types.insert(variant.span, ty.clone());
            }
        }
    }
    let mut declare = |name: &str, kind: &str, span: Span, ty: Option<Type>| {
        let ty = if kind == "type-parameter" {
            ty
        } else {
            types.get(&span).cloned().or(ty)
        };
        facts
            .declarations
            .push(Declaration::new(name, kind, span, ty));
    };
    if let Some(module) = &program.module {
        declare(&module.path, "module", module.span, None);
    }
    for import in &program.imports {
        declare(
            import
                .alias
                .as_deref()
                .unwrap_or_else(|| import.path.rsplit("::").next().unwrap()),
            "import",
            import.span,
            None,
        );
    }
    for item in &program.items {
        match item {
            Item::Function(f) => {
                declare(
                    &f.name,
                    "function",
                    f.span,
                    Some(Type::Fn(
                        f.params.iter().map(|p| p.ty.clone()).collect(),
                        Box::new(f.return_type.clone()),
                    )),
                );
                for p in &f.params {
                    declare(&p.name, "parameter", p.span, Some(p.ty.clone()));
                }
            }
            Item::Class(c) => {
                declare(&c.name, "class", c.span, Some(Type::Named(c.name.clone())));
                for f in &c.fields {
                    declare(
                        &f.name,
                        if f.is_static { "static-field" } else { "field" },
                        f.span,
                        Some(f.ty.clone()),
                    );
                }
                for m in &c.methods {
                    declare(
                        &m.name,
                        "method",
                        m.span,
                        Some(Type::Fn(
                            m.params.iter().map(|p| p.ty.clone()).collect(),
                            Box::new(m.return_type.clone()),
                        )),
                    );
                    for p in &m.params {
                        declare(&p.name, "parameter", p.span, Some(p.ty.clone()));
                    }
                }
                for c in &c.constructors {
                    declare("init", "constructor", c.span, None);
                    for p in &c.params {
                        declare(&p.name, "parameter", p.span, Some(p.ty.clone()));
                    }
                }
            }
            Item::Enum(e) => {
                declare(&e.name, "enum", e.span, Some(Type::Named(e.name.clone())));
                for p in &e.type_params {
                    declare(p, "type-parameter", e.span, None);
                }
                for v in &e.variants {
                    declare(
                        &v.name,
                        "variant",
                        v.span,
                        Some(Type::Named(e.name.clone())),
                    );
                    facts.members.push((
                        e.span,
                        Declaration::new(
                            &v.name,
                            "variant",
                            v.span,
                            Some(Type::Named(e.name.clone())),
                        ),
                    ));
                }
            }
            Item::Interface(i) => {
                declare(
                    &i.name,
                    "interface",
                    i.span,
                    Some(Type::Named(i.name.clone())),
                );
                for p in &i.type_params {
                    declare(p, "type-parameter", i.span, None);
                }
                for m in &i.methods {
                    declare(
                        &m.name,
                        "method",
                        m.span,
                        Some(Type::Fn(
                            m.params.iter().map(|p| p.ty.clone()).collect(),
                            Box::new(m.return_type.clone()),
                        )),
                    );
                    for p in &m.params {
                        declare(&p.name, "parameter", p.span, Some(p.ty.clone()));
                    }
                }
            }
        }
    }
}

pub(super) type Names = HashMap<String, Vec<(usize, usize)>>;
pub(super) fn select(
    span: Span,
    name: &str,
    last: bool,
    names: &HashMap<UnitId, Names>,
    paths: &HashMap<UnitId, String>,
) -> Option<Location> {
    let unit = crate::module::ModuleId(span.file_id.0);
    let name = name.rsplit("::").next()?;
    let occurrences = names.get(&unit)?.get(name)?;
    let start = occurrences.partition_point(|&(start, _)| start < span.start);
    let end = occurrences.partition_point(|&(_, end)| end <= span.end);
    if start >= end {
        return None;
    }
    let (start, end) = occurrences[if last { end - 1 } else { start }];
    Some(Location {
        path: paths.get(&unit)?.clone(),
        start,
        end,
    })
}

pub(super) fn finish(
    captures: &HashMap<UnitId, CapturedUnit>,
    paths: &HashMap<UnitId, String>,
    names: &HashMap<UnitId, Names>,
    functions: &[Function],
) -> (Vec<Symbol>, Vec<Reference>) {
    let mut callable = HashMap::new();
    for f in functions {
        for l in &f.locations {
            callable.insert((l.path.as_str(), l.start, l.end), f.id.as_str());
        }
    }
    let mut symbols = BTreeMap::new();
    let mut register = |d: &Declaration| {
        let location = select(d.span, &d.name, false, names, paths);
        let raw_path = paths.get(&crate::module::ModuleId(d.span.file_id.0));
        let function = if matches!(d.kind.as_str(), "function" | "method" | "constructor") {
            raw_path
                .and_then(|p| callable.get(&(p.as_str(), d.span.start, d.span.end)))
                .copied()
        } else {
            None
        };
        let id = function
            .map(str::to_owned)
            .unwrap_or_else(|| match &location {
                Some(l) => format!("symbol:{}:{}:{}:{}", l.path, l.start, l.end, d.kind),
                None => format!("builtin:{}:{}", d.kind, d.name),
            });
        symbols.entry(id.clone()).or_insert_with(|| Symbol {
            identity: None,
            id: id.clone(),
            name: d.name.clone(),
            kind: d.kind.clone(),
            location,
            ty: d.ty.clone(),
        });
        id
    };
    let mut units: Vec<_> = captures.keys().copied().collect();
    units.sort();
    let mut references = BTreeMap::new();
    let members: HashMap<_, _> = captures
        .values()
        .flat_map(|c| &c.symbols.members)
        .map(|(owner, d)| ((*owner, d.name.as_str(), d.kind.as_str()), d))
        .collect();
    for unit in &units {
        for d in &captures[unit].symbols.declarations {
            register(d);
        }
    }
    for unit in units {
        let facts = &captures[&unit].symbols;
        for r in &facts.member_references {
            if let Some(target) = members.get(&(r.owner, r.name.as_str(), r.kind.as_str())) {
                let target = register(target);
                if let Some(path) = paths.get(&crate::module::ModuleId(r.span.file_id.0)) {
                    // The type checker retains the member's exact parser token;
                    // no name search through the qualifier or payload is needed.
                    let location = Location {
                        path: path.clone(),
                        start: r.span.start,
                        end: r.span.end,
                    };
                    let key = (
                        location.path.clone(),
                        location.start,
                        location.end,
                        target.clone(),
                        "variant".into(),
                    );
                    references.insert(
                        key,
                        Reference {
                            identity: None,
                            target,
                            location,
                            role: "variant".into(),
                        },
                    );
                }
            }
        }
        for r in &facts.references {
            let target = register(&r.target);
            if let Some(location) = select(r.span, &r.written, r.last, names, paths) {
                let key = (
                    location.path.clone(),
                    location.start,
                    location.end,
                    target.clone(),
                    r.role.clone(),
                );
                references.insert(
                    key,
                    Reference {
                        identity: None,
                        target,
                        location,
                        role: r.role.clone(),
                    },
                );
            }
        }
    }
    (
        symbols.into_values().collect(),
        references.into_values().collect(),
    )
}
