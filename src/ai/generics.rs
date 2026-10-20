//! Source declaration provenance retained across monomorphization for AI queries.
use super::*;
use crate::parser::ast::ImportDecl;
use crate::semantic::analysis_symbols::{Declaration, Facts};

#[derive(Default)]
pub(crate) struct Source {
    pub imports: Vec<ImportDecl>,
    pub logical_types: HashMap<String, Type>,
    pub nominal_aliases: HashMap<String, String>,
    pub symbols: Facts,
    written: HashMap<(u32, usize), String>,
    pub(super) declarations: Vec<Declaration>,
    owners: Vec<(Span, String)>,
    details: HashMap<Span, symbols::DeclarationDetails>,
    callables: HashMap<Span, (String, Span)>,
    pub(super) items: HashMap<String, Declaration>,
}
impl Source {
    pub fn capture(program: &Program) -> Self {
        let items: Vec<_> = program
            .items
            .iter()
            .filter(|item| match item {
                Item::Function(f) => !f.type_params.is_empty(),
                Item::Class(c) => !c.type_params.is_empty(),
                _ => false,
            })
            .collect();
        let mut roots = Vec::new();
        for item in &program.items {
            match item {
                Item::Function(f) => roots.push(AstEvent::Block(&f.body)),
                Item::Class(c) => {
                    roots.extend(c.methods.iter().map(|m| AstEvent::Block(&m.body)));
                    roots.extend(c.constructors.iter().map(|m| AstEvent::Block(&m.body)));
                    roots.extend(
                        c.fields
                            .iter()
                            .filter_map(|f| f.initializer.as_ref().map(AstEvent::Expr)),
                    );
                }
                Item::Interface(i) => roots.extend(
                    i.methods
                        .iter()
                        .filter_map(|m| m.default_body.as_ref().map(AstEvent::Block)),
                ),
                _ => {}
            }
        }
        let mut written = HashMap::new();
        for event in roots.into_iter().flat_map(AstWalk::new) {
            match event {
                AstEvent::Expr(Expr::Call(c)) => {
                    written.insert((c.span.file_id.0, c.span.start), c.callee.clone());
                }
                AstEvent::Expr(Expr::New(c)) => {
                    written.insert((c.span.file_id.0, c.span.start), c.class_name.clone());
                }
                _ => {}
            }
        }
        let mut facts = Facts::default();
        symbols::declarations_of(&[], items.iter().copied(), &mut facts, &Default::default());
        let mut callables = HashMap::new();
        for item in &items {
            match item {
                Item::Function(f) => {
                    callables.insert(f.span, (f.name.clone(), f.body.span));
                }
                Item::Class(c) => {
                    for m in &c.methods {
                        callables.insert(m.span, (format!("{}::{}", c.name, m.name), m.body.span));
                    }
                }
                _ => {}
            }
        }
        Self {
            imports: program.imports.clone(),
            logical_types: HashMap::new(),
            nominal_aliases: HashMap::new(),
            symbols: Facts::default(),
            written,
            owners: symbols::owners_of(items.iter().copied()),
            details: symbols::details_of(items.iter().copied(), &Default::default()),
            items: facts
                .declarations
                .iter()
                .filter(|d| matches!(d.kind.as_str(), "function" | "class"))
                .map(|d| (d.name.clone(), d.clone()))
                .collect(),
            declarations: facts.declarations,
            callables,
        }
    }
}

pub(super) fn restore(
    sources: &HashMap<UnitId, Source>,
    captures: &mut HashMap<UnitId, CapturedUnit>,
) {
    if sources.is_empty() {
        return;
    }
    let declarations: HashMap<_, _> = sources
        .values()
        .flat_map(|s| &s.declarations)
        .map(|d| ((d.span, d.kind.as_str(), source_spelling(&d.name)), d))
        .collect();
    let written: HashMap<_, _> = sources.values().flat_map(|s| &s.written).collect();
    // Local generated names can coincide in different modules. Index fully
    // qualified identities globally, and use a leaf only when unambiguous.
    let mut qualified_types = HashMap::new();
    let mut unique_types: HashMap<&str, Option<&Type>> = HashMap::new();
    for source in sources.values() {
        for (generated, ty) in &source.logical_types {
            let identity = match ty {
                Type::Generic(name, _) => name
                    .rsplit_once("::")
                    .map(|(prefix, _)| format!("{prefix}::{generated}")),
                _ => None,
            };
            if let Some(identity) = identity {
                qualified_types.insert(identity, ty);
            }
            unique_types
                .entry(generated.as_str())
                .and_modify(|entry| {
                    if entry.is_some_and(|existing| existing != ty) {
                        *entry = None;
                    }
                })
                .or_insert(Some(ty));
        }
    }
    // Alias names hash their module-qualified identity, the recorded value, so
    // units cannot disagree and one global index serves every unit, including
    // instances checked outside the alias's creator.
    let aliases: HashMap<&str, &str> = sources
        .values()
        .flat_map(|s| &s.nominal_aliases)
        .map(|(alias, name)| (alias.as_str(), name.as_str()))
        .collect();
    let imports: HashMap<_, _> = sources
        .values()
        .flat_map(|s| &s.imports)
        .map(|i| {
            let name = i
                .alias
                .as_deref()
                .unwrap_or_else(|| i.path.rsplit("::").next().unwrap());
            (i.span, Declaration::new(name, "import", i.span, None))
        })
        .collect();
    let owners: HashMap<_, _> = sources.values().flat_map(|s| &s.owners).cloned().collect();
    for (&unit, capture) in captures.iter_mut() {
        if let Some(source) = sources.get(&unit) {
            // Template facts go first: a template-body declaration then keeps
            // its source type (`T`) instead of whichever instance registered
            // first, since instances of one template may disagree.
            let mut symbols = source.symbols.clone();
            symbols.extend(std::mem::take(&mut capture.symbols));
            capture.symbols = symbols;
        }
        let normalize = |ty: &Type| {
            ty.map_names(|name| {
                aliases
                    .get(name.as_str())
                    .map_or_else(|| name.clone(), |&source| source.to_owned())
            })
            .substitute_names(|name| {
                sources
                    .get(&unit)
                    .and_then(|s| s.logical_types.get(name))
                    .or_else(|| qualified_types.get(name).copied())
                    .or_else(|| unique_types.get(name.as_str()).copied().flatten())
                    .cloned()
            })
            .map_names(|name| {
                name.split("::")
                    .map(|segment| {
                        segment
                            .split_once("$mono$")
                            .map_or(segment, |(source, _)| source)
                    })
                    .collect::<Vec<_>>()
                    .join("::")
            })
        };
        let restore = |d: &mut Declaration| {
            if d.kind == "import"
                && let Some(source) = imports.get(&d.span)
            {
                *d = source.clone();
            }
            if let Some(source) =
                declarations.get(&(d.span, d.kind.as_str(), source_spelling(&d.name)))
            {
                *d = (*source).clone();
            }
            d.name = source_spelling(&d.name);
            if let Some(ty) = &mut d.ty {
                *ty = normalize(ty);
            }
        };
        for d in &mut capture.symbols.declarations {
            restore(d);
        }
        for (_, d) in &mut capture.symbols.members {
            restore(d);
        }
        capture.symbols.references.retain(|r| {
            // Expanding an unqualified generic call into a static module call
            // invents a namespace-type use at the same source token. It is
            // compiler provenance, not a second source symbol for that token.
            !(r.target.kind == "builtin-type"
                && crate::semantic::ids::has_internal_module_prefix(&r.written)
                && written
                    .get(&(r.span.file_id.0, r.span.start))
                    .is_some_and(|source| !source.contains("::")))
        });
        for r in &mut capture.symbols.references {
            let generic_callable = matches!(r.target.kind.as_str(), "function" | "method")
                && declarations.contains_key(&(
                    r.target.span,
                    r.target.kind.as_str(),
                    source_spelling(&r.target.name),
                ));
            restore(&mut r.target);
            if let Some(ty) = &mut r.ty {
                *ty = normalize(ty);
            }
            if generic_callable && r.role == "member" {
                r.role = "call".into();
            }
            r.written = if (r.written.contains("$mono$")
                || crate::semantic::ids::has_internal_module_prefix(&r.written))
                && let Some(source) = written.get(&(r.span.file_id.0, r.span.start))
            {
                (*source).clone()
            } else {
                source_spelling(&r.written)
            };
        }
        capture.semantic.map_types(normalize);
        for (span, owner) in &mut capture.symbol_owners {
            if let Some(source) = owners.get(span) {
                owner.clone_from(source);
            }
        }
        if let Some(source) = sources.get(&unit) {
            let import_spans: std::collections::HashSet<_> =
                source.imports.iter().map(|i| i.span).collect();
            capture
                .symbols
                .declarations
                .retain(|d| d.kind != "import" || !import_spans.contains(&d.span));
            for import in &source.imports {
                let name = import
                    .alias
                    .as_deref()
                    .unwrap_or_else(|| import.path.rsplit("::").next().unwrap());
                capture.symbols.declarations.push(Declaration::new(
                    name,
                    "import",
                    import.span,
                    None,
                ));
            }
            capture.symbol_details.extend(source.details.clone());
            capture.symbol_owners.extend(source.owners.clone());
            capture
                .symbols
                .declarations
                .extend(source.declarations.clone());
            // Uninstantiated templates still have a source symbol and callable
            // identity. Effects remain unknown until a checked instance exists.
            let instantiated: std::collections::HashSet<_> = capture
                .declarations
                .values()
                .flatten()
                .map(|(s, _)| *s)
                .collect();
            for (&span, (name, body)) in &source.callables {
                if !instantiated.contains(&span) {
                    let id = callable(name);
                    capture.templates.insert(id);
                    capture.declarations.insert(id, vec![(span, *body)]);
                    capture.graph.merge(
                        id,
                        crate::semantic::call_graph::CallSites {
                            has_unknown: true,
                            ..Default::default()
                        },
                    );
                }
            }
        }
        for details in capture.symbol_details.values_mut() {
            for ty in details
                .extends
                .iter_mut()
                .chain(&mut details.implements)
                .chain(&mut details.payload)
            {
                *ty = normalize(ty);
            }
        }
    }
}
/// Unresolved reason of a never-instantiated template: its effects are
/// unknown, but it never runs, so it cannot call anything at runtime.
pub(super) const UNINSTANTIATED: &str = "uninstantiated-generic-template";

fn callable(name: &str) -> FunctionId {
    name.rsplit_once("::").map_or_else(
        || FunctionId::free(name),
        |(owner, name)| FunctionId::method(TypeId::local(owner), name),
    )
}
/// Compiler generated spelling consists of qualified segments; erase only the
/// specialization suffix, retaining method/module segments.
fn source_spelling(name: &str) -> String {
    if let Some(lambda) = name.strip_prefix("<lambda ")
        && let Some((owner, location)) = lambda.rsplit_once('@')
    {
        return format!("<lambda {}@{location}", source_spelling(owner));
    }
    name.split("::")
        .filter(|s| !s.starts_with("$pkg") && *s != "$local")
        .map(|s| s.split_once("$mono$").map_or(s, |(source, _)| source))
        .collect::<Vec<_>>()
        .join("::")
}

/// Merge instantiations into their source functions. Returns each original
/// id's merged id, or nothing when no function was renamed.
pub(super) fn aggregate(
    sources: &HashMap<UnitId, Source>,
    paths: &HashMap<UnitId, String>,
    functions: &mut Vec<Function>,
    semantic: &mut SemanticFacts,
) -> HashMap<String, String> {
    if sources.is_empty() && !functions.iter().any(|f| f.name.contains("$mono$")) {
        return HashMap::new();
    }
    let callables: HashMap<_, _> = sources
        .values()
        .flat_map(|s| &s.callables)
        .filter_map(|(span, (name, _))| {
            Some((
                (
                    paths
                        .get(&crate::module::ModuleId(span.file_id.0))?
                        .as_str(),
                    span.start,
                    span.end,
                ),
                name,
            ))
        })
        .collect();
    let mut ids = HashMap::new();
    for f in functions.iter_mut() {
        let original_name = f.name.clone();
        let name = f
            .locations
            .iter()
            .find_map(|l| callables.get(&(l.path.as_str(), l.start, l.end)).copied());
        if let Some(name) = name {
            f.name.clone_from(name);
        } else {
            f.name = source_spelling(&f.name);
        }
        let id = if f.name == original_name {
            f.id.clone()
        } else {
            hash(serde_json::to_vec(&(&f.module, &f.name)).unwrap())
        };
        ids.insert(f.id.clone(), id.clone());
        f.id = id;
    }
    let remap = |id: &mut String| {
        #[cfg(test)]
        REMAPS.with(|count| count.set(count.get() + 1));
        if let Some(source) = ids.get(id) {
            id.clone_from(source);
        }
    };
    let mut merged = BTreeMap::<String, Function>::new();
    for mut f in functions.drain(..) {
        for id in &mut f.callees {
            remap(id);
        }
        for unresolved in &mut f.unresolved {
            *unresolved = unresolved.rsplit_once('#').map_or_else(
                || source_spelling(unresolved),
                |(path, name)| format!("{path}#{}", source_spelling(name)),
            );
        }
        match merged.entry(f.id.clone()) {
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(f);
            }
            std::collections::btree_map::Entry::Occupied(mut e) => {
                let source = e.get_mut();
                source.locations.extend(f.locations);
                source.rename_calls.extend(f.rename_calls);
                source.rename_imports.extend(f.rename_imports);
                source.callees.extend(f.callees);
                source.runtime_effects |= f.runtime_effects;
                source.unknown |= f.unknown;
                source.synthetic &= f.synthetic;
                source.unresolved.extend(f.unresolved);
            }
        }
    }
    *functions = merged.into_values().collect();
    for f in functions.iter_mut() {
        f.callees.sort();
        f.callees.dedup();
        for locations in [&mut f.rename_calls, &mut f.rename_imports] {
            locations.sort_by(|a, b| (&a.path, a.start, a.end).cmp(&(&b.path, b.start, b.end)));
            locations.dedup();
        }
    }
    let operation_spelling = |operation: &mut String| {
        if let Some((prefix @ ("call" | "static" | "method"), name)) = operation.split_once(':') {
            *operation = format!("{prefix}:{}", source_spelling(name));
        }
    };
    for e in &mut semantic.expressions {
        remap(&mut e.function);
        if let Some(operation) = &mut e.operation {
            operation_spelling(operation);
        }
        if let Some(target) = &mut e.target {
            remap(target);
        }
    }
    for f in &mut semantic.flows {
        remap(&mut f.function);
    }
    let mut effects = BTreeMap::new();
    for (mut id, bits) in std::mem::take(&mut semantic.compiler_effects) {
        remap(&mut id);
        *effects.entry(id).or_default() |= bits;
    }
    semantic.compiler_effects = effects;
    // Every instantiation reports the same source call sites; key them by the
    // merged source function and keep one entry per site.
    let mut unresolved = BTreeMap::<String, Vec<semantic::UnresolvedCall>>::new();
    for (mut id, calls) in std::mem::take(&mut semantic.unresolved_calls) {
        remap(&mut id);
        unresolved.entry(id).or_default().extend(calls);
    }
    for calls in unresolved.values_mut() {
        for call in calls.iter_mut() {
            operation_spelling(&mut call.operation);
        }
        fn key(c: &semantic::UnresolvedCall) -> (&str, usize, usize, &str) {
            let l = &c.location;
            (&l.path, l.start, l.end, &c.operation)
        }
        calls.sort_by(|a, b| key(a).cmp(&key(b)));
        calls.dedup_by(|a, b| key(a) == key(b));
    }
    semantic.unresolved_calls = unresolved;
    let mut witnesses = BTreeMap::<String, Vec<serde_json::Value>>::new();
    for (mut id, mut values) in std::mem::take(&mut semantic.witnesses) {
        remap(&mut id);
        for value in &mut values {
            if let Some(owner) = value["witness"]["owner"].as_str() {
                let mut owner = owner.to_owned();
                remap(&mut owner);
                owner = source_spelling(&owner);
                value["witness"]["owner"] = serde_json::json!(owner);
            }
        }
        witnesses.entry(id).or_default().extend(values);
    }
    // Channel operations are source sites, not one observation per generic
    // instantiation. Preserve source order with an expected-linear hash pass.
    for values in witnesses.values_mut() {
        let mut seen = std::collections::HashSet::new();
        values.retain(|value| {
            let cause = &value["witness"]["cause"];
            if !cause["operation"]
                .as_str()
                .is_some_and(|op| op.starts_with("channel-"))
            {
                return true;
            }
            seen.insert(cause.to_string())
        });
    }
    semantic.witnesses = witnesses;
    ids
}

/// Source references in a never-instantiated body still form caller edges.
/// Declaration ranges are disjoint and references already have source order;
/// a merge sweep avoids scanning every template for every call.
pub(super) fn source_edges(
    sources: &HashMap<UnitId, Source>,
    paths: &HashMap<UnitId, String>,
    functions: &mut [Function],
    references: &[symbols::Reference],
) -> usize {
    if sources.is_empty() {
        return 0;
    }
    let spans: std::collections::HashSet<_> = sources
        .values()
        .flat_map(|s| s.callables.keys())
        .filter_map(|span| {
            Some((
                paths
                    .get(&crate::module::ModuleId(span.file_id.0))?
                    .as_str(),
                span.start,
                span.end,
            ))
        })
        .collect();
    let mut ranges: Vec<_> = functions
        .iter()
        .enumerate()
        .filter(|(_, f)| f.body_id.is_none())
        .flat_map(|(i, f)| {
            let spans = &spans;
            f.locations
                .iter()
                .filter(move |l| spans.contains(&(l.path.as_str(), l.start, l.end)))
                .map(move |l| (l.clone(), i))
        })
        .collect();
    if ranges.is_empty() {
        return 0;
    }
    ranges.sort_unstable_by(|a, b| (&a.0.path, a.0.start).cmp(&(&b.0.path, b.0.start)));
    let callable_ids: std::collections::HashSet<_> =
        functions.iter().map(|f| f.id.as_str()).collect();
    let mut pending = Vec::new();
    let mut range = 0;
    let mut visits = 0;
    for reference in references {
        visits += 1;
        let location = &reference.location;
        while ranges.get(range).is_some_and(|(l, _)| {
            l.path < location.path || (l.path == location.path && l.end <= location.start)
        }) {
            visits += 1;
            range += 1;
        }
        if let Some((l, caller)) = ranges.get(range)
            && l.path == location.path
            && l.start <= location.start
            && location.end <= l.end
            && matches!(reference.role.as_str(), "call" | "member")
            && callable_ids.contains(reference.target.as_str())
        {
            pending.push((*caller, reference.target.clone()));
        }
    }
    drop(callable_ids);
    let mut changed = std::collections::HashSet::new();
    for (caller, target) in pending {
        functions[caller].callees.push(target);
        changed.insert(caller);
    }
    for caller in changed {
        functions[caller].callees.sort_unstable();
        functions[caller].callees.dedup();
    }
    visits
}

/// Remove private nominal namespaces only after package identities are attached,
/// so dependency types retain their public package prefix.
pub(super) fn present_types(snapshot: &mut Snapshot) {
    let names = display::TypeNames::new(snapshot);
    let modules: HashMap<_, _> = snapshot
        .functions
        .iter()
        .map(|f| {
            (
                f.id.as_str(),
                f.identity.as_ref().map(|i| i.module.as_str()),
            )
        })
        .collect();
    let present = |ty: &Type, module| {
        ty.map_names(|name| {
            let public = names.name(name, module);
            if public.contains("$pkg") {
                source_spelling(&public)
            } else {
                public
            }
        })
    };
    for symbol in &mut snapshot.semantic.symbols {
        let module = symbol.identity.as_ref().map(|i| i.module.as_str());
        if let Some(ty) = &mut symbol.ty {
            *ty = present(ty, module);
        }
        for ty in symbol
            .details
            .extends
            .iter_mut()
            .chain(&mut symbol.details.implements)
            .chain(&mut symbol.details.payload)
        {
            *ty = present(ty, module);
        }
    }
    for expression in &mut snapshot.semantic.expressions {
        if let Some(ty) = &mut expression.ty {
            *ty = present(
                ty,
                modules.get(expression.function.as_str()).copied().flatten(),
            );
        }
    }
}

#[cfg(test)]
thread_local! { static REMAPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operation_names_preserve_prefixes_and_scale_per_expression() {
        for count in [1, 16, 256, 4096] {
            let cases = [
                ("call:fold$mono$abc", "call:fold"),
                ("static:$pkg123::coll::fold$mono$abc", "static:coll::fold"),
                ("static:$local::coll::fold$mono$abc", "static:coll::fold"),
                ("method:push$mono$abc", "method:push"),
                ("io:print", "io:print"),
                ("write:field", "write:field"),
                ("wait:await", "wait:await"),
                ("allocate:new", "allocate:new"),
            ];
            let mut semantic = SemanticFacts::default();
            for _ in 0..count {
                for (operation, _) in cases {
                    semantic.expressions.push(
                        serde_json::from_value(serde_json::json!({
                            "function":"f", "location":{"path":"m.wi","start":0,"end":1},
                            "ty":null,"target":null,"operation":operation,"fingerprint":"same",
                            "flow":0,"node":0,"occurrences":[0]
                        }))
                        .unwrap(),
                    );
                }
            }
            // Source metadata enables aggregation even without concrete functions.
            let sources = HashMap::from([(crate::module::ModuleId(0), Source::default())]);
            REMAPS.with(|visits| visits.set(0));
            aggregate(&sources, &HashMap::new(), &mut vec![], &mut semantic);
            assert_eq!(REMAPS.with(|visits| visits.get()), count * cases.len());
            for (i, expression) in semantic.expressions.iter().enumerate() {
                assert_eq!(
                    expression.operation.as_deref(),
                    Some(cases[i % cases.len()].1)
                );
            }
            println!(
                "expressions={} expression_visits={}",
                count * cases.len(),
                REMAPS.with(|visits| visits.get())
            );
        }
    }
    #[test]
    fn source_names_preserve_method_segments_and_lambda_locations() {
        assert_eq!(
            source_spelling("$pkg123::coll::Ring$mono$abc::push"),
            "coll::Ring::push"
        );
        assert_eq!(
            source_spelling("<lambda fold$mono$abc@10:20>"),
            "<lambda fold@10:20>"
        );
        assert_ne!(
            source_spelling("<lambda fold$mono$abc@10:20>"),
            source_spelling("<lambda fold$mono$abc@30:40>")
        );
        let mut functions: Vec<Function> = vec![serde_json::from_value(serde_json::json!({
            "id":"existing-dispatch-id", "module":"m.wi", "name":"C::$dispatch", "locations":[],
            "synthetic":true, "fingerprint":"", "body_fingerprint":"", "callees":[], "runtime_effects":0, "unknown":false, "unresolved":[]
        })).unwrap()];
        aggregate(
            &HashMap::new(),
            &HashMap::new(),
            &mut functions,
            &mut SemanticFacts::default(),
        );
        assert_eq!(functions[0].id, "existing-dispatch-id");
    }
    #[test]
    fn uninstantiated_source_edges_sweep_references_once() {
        for n in [16, 64, 256, 1024] {
            let path = "source.wi";
            let mut source = Source::default();
            let mut functions: Vec<Function> = (0..n).map(|i| {
                let span = Span::new(i * 10, i * 10 + 9, 1, 1);
                source.callables.insert(span, (format!("f{i}"), span));
                serde_json::from_value(serde_json::json!({
                    "id":format!("f{i}"), "module":path, "name":format!("f{i}"),
                    "locations":[{"path":path,"start":span.start,"end":span.end}], "synthetic":false,
                    "fingerprint":"", "body_fingerprint":"", "callees":[], "runtime_effects":0, "unknown":true, "unresolved":[]
                })).unwrap()
            }).collect();
            let references: Vec<_> = (0..n)
                .flat_map(|i| {
                    [
                        symbols::Reference {
                            identity: None,
                            target: "f0".into(),
                            role: "call".into(),
                            ty: None,
                            location: Location {
                                path: path.into(),
                                start: i * 10 + 5,
                                end: i * 10 + 6,
                            },
                        },
                        // The end boundary is outside the declaration. It must never
                        // add the unrelated target to either adjacent caller.
                        symbols::Reference {
                            identity: None,
                            target: "f1".into(),
                            role: "call".into(),
                            ty: None,
                            location: Location {
                                path: path.into(),
                                start: i * 10 + 9,
                                end: i * 10 + 10,
                            },
                        },
                    ]
                })
                .collect();
            let visits = source_edges(
                &HashMap::from([(UnitId::ENTRY, source)]),
                &HashMap::from([(UnitId::ENTRY, path.into())]),
                &mut functions,
                &references,
            );
            assert_eq!(visits, 3 * n);
            assert!(functions.iter().all(|f| f.callees == ["f0"]));
            println!(
                "unused_bodies={n} references={} sweep_visits={visits} caller_edges={n}",
                references.len()
            );
        }
    }
    #[test]
    fn unresolved_calls_follow_merged_identities_once_per_site() {
        for declarations in [1, 8, 64] {
            for instances in [1, 8, 64] {
                let sites = 4;
                let n = declarations * instances;
                let mut functions: Vec<Function> = (0..n).map(|i| serde_json::from_value(serde_json::json!({
                    "id":format!("f{i}"), "module":"m.wi", "name":format!("f{}$mono${i}", i / instances),
                    "locations":[{"path":"m.wi","start": i / instances * 10,"end": i / instances * 10+9}],
                    "synthetic":false, "fingerprint":"same", "body_fingerprint":"same",
                    "callees":[], "runtime_effects":0, "unknown":true, "unresolved":["indirect-or-unresolved"]
                })).unwrap()).collect();
                let mut semantic = SemanticFacts::default();
                for i in 0..n {
                    let start = i / instances * 10;
                    let calls = (0..sites)
                        .rev()
                        .map(|s| {
                            serde_json::from_value(serde_json::json!({
                                "operation": if s == 0 { format!("call:g$mono${i}") } else { "f".to_owned() },
                                "location":{"path":"m.wi","start":start + s,"end":start + s + 1}
                            }))
                            .unwrap()
                        })
                        .collect();
                    semantic.unresolved_calls.insert(format!("f{i}"), calls);
                }
                REMAPS.with(|count| count.set(0));
                aggregate(
                    &HashMap::new(),
                    &HashMap::new(),
                    &mut functions,
                    &mut semantic,
                );
                let ids: std::collections::HashSet<_> =
                    functions.iter().map(|f| f.id.as_str()).collect();
                assert_eq!(semantic.unresolved_calls.len(), declarations);
                for (owner, calls) in &semantic.unresolved_calls {
                    assert!(ids.contains(owner.as_str()));
                    assert_eq!(calls.len(), sites);
                    assert!(
                        calls
                            .windows(2)
                            .all(|w| w[0].location.start < w[1].location.start)
                    );
                    assert_eq!(calls[0].operation, "call:g");
                    assert!(calls.iter().skip(1).all(|c| c.operation == "f"));
                }
                // One remap per input owner; callees and other facts are empty.
                let remaps = REMAPS.with(|count| count.get());
                assert_eq!(remaps, n);
                println!(
                    "declarations={declarations} instances={instances} input_sites={} owner_remaps={remaps} output_sites={}",
                    n * sites,
                    semantic
                        .unresolved_calls
                        .values()
                        .map(Vec::len)
                        .sum::<usize>()
                );
            }
        }
    }
    #[test]
    fn aggregation_visits_each_instantiation_evidence_once() {
        for declarations in [1, 8, 64] {
            for instances in [1, 8, 64] {
                let n = declarations * instances;
                let mut functions: Vec<Function> = (0..n).map(|i| serde_json::from_value(serde_json::json!({
                    "id":format!("f{i}"), "module":"m.wi", "name":format!("f{}$mono${i}", i / instances),
                    "locations":[{"path":"m.wi","start": i / instances * 10,"end": i / instances * 10+9}],
                    "synthetic":false, "fingerprint":"same", "body_fingerprint":"same",
                    "callees":[format!("f{}", (i + 1) % n)], "runtime_effects":1 << (i % 4), "unknown":false, "unresolved":[]
                })).unwrap()).collect();
                let mut semantic = SemanticFacts::default();
                for i in 0..n {
                    semantic
                        .compiler_effects
                        .insert(format!("f{i}"), 1 << (i % 4));
                    semantic.witnesses.insert(
                        format!("f{i}"),
                        vec![serde_json::json!({"witness":{"owner":format!("f{i}")}})],
                    );
                }
                REMAPS.with(|count| count.set(0));
                aggregate(
                    &HashMap::new(),
                    &HashMap::new(),
                    &mut functions,
                    &mut semantic,
                );
                assert_eq!(functions.len(), declarations);
                assert_eq!(semantic.compiler_effects.len(), declarations);
                assert_eq!(semantic.witnesses.values().map(Vec::len).sum::<usize>(), n);
                let ids: std::collections::HashSet<_> =
                    functions.iter().map(|f| f.id.as_str()).collect();
                for f in &functions {
                    assert!(!f.name.contains("$mono$"));
                    assert!(f.callees.iter().all(|id| ids.contains(id.as_str())));
                    if instances >= 4 {
                        assert_eq!(f.runtime_effects, 15);
                    }
                }
                let remaps = REMAPS.with(|count| count.get());
                assert_eq!(remaps, 4 * n);
                println!(
                    "declarations={declarations} instances={instances} input_nodes={n} id_remaps={remaps} output_nodes={}",
                    functions.len()
                );
            }
        }
    }
}
