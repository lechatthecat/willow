//! Serialization of compiler-owned declaration/reference evidence.
use super::*;
use crate::semantic::analysis_symbols::{Declaration, Facts};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Symbol {
    #[serde(default)]
    pub identity: Option<SymbolIdentity>,
    pub id: String,
    pub name: String,
    /// Live compiler owner spelling for standalone direct selectors. This is
    /// display metadata, not an additional persisted semantic fact.
    #[serde(skip)]
    pub source_name: Option<String>,
    #[serde(skip)]
    pub details: DeclarationDetails,
    pub kind: String,
    pub location: Option<Location>,
    pub ty: Option<Type>,
}
/// Live declaration presentation metadata, captured before bodies are offloaded.
/// It does not alter persisted symbol facts or selector identities.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeclarationDetails {
    pub is_async: bool,
    pub type_params: Vec<String>,
}

pub(super) fn details(program: &Program) -> HashMap<Span, DeclarationDetails> {
    let mut result = HashMap::new();
    for item in &program.items {
        match item {
            Item::Function(f) if f.is_async => {
                result.insert(
                    f.span,
                    DeclarationDetails {
                        is_async: true,
                        ..Default::default()
                    },
                );
            }
            Item::Class(c) => {
                for m in &c.methods {
                    if m.is_async && c.span.contains(m.span) {
                        result.insert(
                            m.span,
                            DeclarationDetails {
                                is_async: true,
                                ..Default::default()
                            },
                        );
                    }
                }
            }
            Item::Interface(i) if !i.type_params.is_empty() => {
                result.insert(
                    i.span,
                    DeclarationDetails {
                        type_params: i.type_params.clone(),
                        ..Default::default()
                    },
                );
            }
            Item::Enum(e) if !e.type_params.is_empty() => {
                result.insert(
                    e.span,
                    DeclarationDetails {
                        type_params: e.type_params.clone(),
                        ..Default::default()
                    },
                );
            }
            _ => {}
        }
    }
    result
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
                for (ordinal, init) in c.constructors.iter().enumerate() {
                    if c.span.contains(init.span) {
                        owners.push((init.span, format!("{}::init#{ordinal}", c.name)));
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
    let ranges: Vec<_> = captures
        .values()
        .flat_map(|unit| &unit.symbol_owners)
        .filter_map(|(span, owner)| {
            Some((
                Location {
                    path: paths.get(&crate::module::ModuleId(span.file_id.0))?.clone(),
                    start: span.start,
                    end: span.end,
                },
                owner.clone(),
            ))
        })
        .collect();
    let declaration_key = |d: &Declaration| {
        (
            d.span.file_id.0,
            d.span.start,
            d.span.end,
            d.kind.clone(),
            d.name.rsplit("::").next().unwrap_or(&d.name).to_owned(),
        )
    };
    let declarations: HashMap<_, _> = captures
        .values()
        .flat_map(|unit| {
            unit.symbols
                .declarations
                .iter()
                .chain(unit.symbols.members.iter().map(|(_, d)| d))
                .chain(unit.symbols.references.iter().map(|r| &r.target))
        })
        .map(|declaration| (declaration_key(declaration), declaration))
        .collect();
    let mut declarations: Vec<_> = declarations.into_values().collect();
    declarations.sort_by_cached_key(|d| declaration_key(d));
    let points: Vec<_> = declarations
        .iter()
        .map(|d| Location {
            path: paths
                .get(&crate::module::ModuleId(d.span.file_id.0))
                .cloned()
                .unwrap_or_default(),
            start: d.span.start,
            end: d.span.end,
        })
        .collect();
    let owners = crate::compiler_db::references::owners(&points, &ranges);
    let mut ordinals = HashMap::<(String, String, String), usize>::new();
    let mut identities = HashMap::new();
    let mut source_names = HashMap::new();
    for (d, owner) in declarations.into_iter().zip(owners) {
        // JSON escapes (notably Windows path separators) cannot deserialize
        // into borrowed str. Skip the unused path and own the decoded name.
        let (_, owner_name): (serde::de::IgnoredAny, String) =
            serde_json::from_str(&owner).expect("compiler owner identity");
        let name = d.name.rsplit("::").next().unwrap_or(&d.name);
        let source_name = if matches!(
            d.kind.as_str(),
            "field" | "static-field" | "variant" | "parameter" | "binding" | "type-parameter"
        ) && owner_name != "module"
        {
            format!("{owner_name}::{name}")
        } else if matches!(d.kind.as_str(), "method" | "constructor") && owner_name != "module" {
            owner_name
        } else {
            name.to_owned()
        };
        source_names.insert(declaration_key(d), source_name);
        let key = (
            owner,
            d.kind.clone(),
            d.name.rsplit("::").next().unwrap_or(&d.name).to_owned(),
        );
        let ordinal = ordinals.entry(key.clone()).or_default();
        identities.insert(
            declaration_key(d),
            format!(
                "symbol:{}",
                hash_serialized(&(key, *ordinal)).expect("symbol identity serializes")
            ),
        );
        *ordinal += 1;
    }
    let mut symbols = BTreeMap::new();
    let mut register = |d: &Declaration| {
        let location = select(d.span, &d.name, false, names, paths);
        let raw_path = paths.get(&crate::module::ModuleId(d.span.file_id.0));
        // Constructor overloads share a lowered dispatch function but remain distinct
        // source declarations. Keep their compiler owner/ordinal identities.
        let function = if matches!(d.kind.as_str(), "function" | "method") {
            raw_path
                .and_then(|p| callable.get(&(p.as_str(), d.span.start, d.span.end)))
                .copied()
        } else {
            None
        };
        let id = function
            .map(str::to_owned)
            .unwrap_or_else(|| match &location {
                Some(_) => identities[&declaration_key(d)].clone(),
                None => format!("builtin:{}:{}", d.kind, d.name),
            });
        symbols.entry(id.clone()).or_insert_with(|| Symbol {
            identity: None,
            id: id.clone(),
            name: d.name.clone(),
            source_name: source_names.get(&declaration_key(d)).cloned(),
            kind: d.kind.clone(),
            // Type-parameter declarations share their container span. Do not
            // clone the whole generic parameter list onto each parameter.
            details: if matches!(
                d.kind.as_str(),
                "function" | "method" | "interface" | "enum"
            ) {
                captures
                    .get(&crate::module::ModuleId(d.span.file_id.0))
                    .and_then(|unit| unit.symbol_details.get(&d.span))
                    .cloned()
                    .unwrap_or_default()
            } else {
                DeclarationDetails::default()
            },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_metadata_is_stored_once_per_container_not_per_parameter() {
        for n in [1, 16, 256] {
            let span = Span::new(0, n * 8 + 8, 1, 1);
            let params: Vec<_> = (0..n).map(|i| format!("T{i}")).collect();
            let mut facts = Facts::default();
            facts
                .declarations
                .push(Declaration::new("Source", "interface", span, None));
            for param in &params {
                facts
                    .declarations
                    .push(Declaration::new(param, "type-parameter", span, None));
            }
            let captured = CapturedUnit {
                graph: CallGraph::default(),
                semantic: semantic::Captured::default(),
                symbols: facts,
                symbol_owners: vec![(span, "Source".into())],
                symbol_details: HashMap::from([(
                    span,
                    DeclarationDetails {
                        is_async: false,
                        type_params: params.clone(),
                    },
                )]),
                declarations: HashMap::new(),
                identities: HashMap::new(),
                lambda_names: HashMap::new(),
                rename_calls: Vec::new(),
                direct: HashMap::new(),
                dispatches: HashMap::new(),
                dispatch_sites: HashMap::new(),
            };
            let names = std::iter::once(("Source".into(), vec![(0, 6)]))
                .chain(
                    params
                        .iter()
                        .enumerate()
                        .map(|(i, p)| (p.clone(), vec![(8 + i * 8, 8 + i * 8 + p.len())])),
                )
                .collect();
            let (symbols, _) = finish(
                &HashMap::from([(UnitId::ENTRY, captured)]),
                &HashMap::from([(UnitId::ENTRY, "/tmp/generic.wi".into())]),
                &HashMap::from([(UnitId::ENTRY, names)]),
                &[],
            );
            assert_eq!(symbols.len(), n + 1);
            assert_eq!(
                symbols
                    .iter()
                    .map(|s| s.details.type_params.len())
                    .sum::<usize>(),
                n
            );
            assert_eq!(
                symbols
                    .iter()
                    .find(|s| s.kind == "interface")
                    .unwrap()
                    .details
                    .type_params,
                params
            );
            println!("generic_parameters={n} stored_parameter_names={n}");
        }
    }

    #[test]
    fn finish_accepts_escaped_owner_paths_and_names() {
        let paths = [
            "/tmp/project/main.wi",
            r"C:\Users\runneradmin\project\main.wi",
            r"\\?\C:\Users\runneradmin\project\main.wi",
            r"\\server\share\project\main.wi",
            "/tmp/quoted\"project/main.wi",
            "/tmp/tab\tnewline\n/main.wi",
            "/tmp/日本語/main.wi",
        ];
        let mut ids = std::collections::HashSet::new();
        for path in paths {
            for owner in [
                None,
                Some("run"),
                Some("Widget::run"),
                Some("escaped\"\\\nowner"),
            ] {
                let span = Span::new(2, 3, 1, 3);
                let mut facts = Facts::default();
                facts
                    .declarations
                    .push(Declaration::new("x", "binding", span, Some(Type::I64)));
                facts.reference(span, "x", facts.declarations[0].clone(), "read", false);
                let captured = CapturedUnit {
                    graph: CallGraph::default(),
                    semantic: semantic::Captured::default(),
                    symbols: facts,
                    symbol_owners: owner
                        .map(|name| vec![(Span::new(0, 8, 1, 1), name.into())])
                        .unwrap_or_default(),
                    symbol_details: HashMap::new(),
                    declarations: HashMap::new(),
                    identities: HashMap::new(),
                    lambda_names: HashMap::new(),
                    rename_calls: Vec::new(),
                    direct: HashMap::new(),
                    dispatches: HashMap::new(),
                    dispatch_sites: HashMap::new(),
                };
                let captures = HashMap::from([(UnitId::ENTRY, captured)]);
                let paths = HashMap::from([(UnitId::ENTRY, path.into())]);
                let names =
                    HashMap::from([(UnitId::ENTRY, HashMap::from([("x".into(), vec![(2, 3)])]))]);
                let (symbols, references) = finish(&captures, &paths, &names, &[]);
                assert_eq!(symbols.len(), 1);
                assert_eq!(references.len(), 1);
                let expected = owner.map_or_else(|| "x".to_owned(), |name| format!("{name}::x"));
                assert_eq!(symbols[0].source_name.as_deref(), Some(expected.as_str()));
                assert_eq!(symbols[0].location.as_ref().unwrap().path, path);
                assert_eq!(references[0].target, symbols[0].id);
                assert!(
                    ids.insert(symbols[0].id.clone()),
                    "path/owner identities collided"
                );
                assert_eq!(
                    finish(&captures, &paths, &names, &[]),
                    (symbols, references)
                );
            }
        }
    }
}
