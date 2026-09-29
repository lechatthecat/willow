//! Live semantic rename relationships and indexed source patches.
use super::*;
use crate::lexer::{Lexer, token::TokenKind};
use crate::semantic::{analysis_symbols::Declaration, symbols::SymbolTable};
use edit::{EditWork, Patch, Rejection, RejectionLocation};
use std::collections::HashSet;

pub(super) fn validate_name(name: &str) -> Result<()> {
    let tokens = Lexer::new(name)
        .tokenize()
        .map_err(|_| anyhow::anyhow!("invalid identifier"))?;
    if tokens.len() == 2 {
        if let Some(keyword) = tokens[0].kind.keyword_name() {
            anyhow::bail!("rename requires an identifier; '{keyword}' is a reserved keyword");
        }
        if matches!(&tokens[0].kind, TokenKind::Ident(s) if s == name) {
            return Ok(());
        }
    }
    anyhow::bail!("rename requires an identifier")
}

#[derive(Default)]
pub(super) struct Relations {
    classes: Vec<Class>,
    interfaces: Vec<(Declaration, Declaration)>,
}
struct Class {
    span: Span,
    base: Option<Span>,
    methods: Vec<(String, Span, bool)>,
    members: Vec<(String, Span)>,
    contracts: Vec<(String, Span)>,
}

pub(super) fn relations(program: &Program, checked: &SymbolTable) -> Relations {
    let mut result = Relations::default();
    for item in &program.items {
        match item {
            Item::Class(c) => {
                let Some(info) = checked.lookup_class(&c.name) else {
                    continue;
                };
                let base = info
                    .base_class
                    .as_ref()
                    .and_then(|base| checked.lookup_class(base))
                    .map(|base| base.declaration_span);
                let mut contracts = Vec::new();
                for implemented in &info.implements {
                    let (Type::Named(name) | Type::Generic(name, _)) = implemented else {
                        continue;
                    };
                    if let Some(interface) = checked.lookup_interface(name) {
                        contracts.extend(
                            interface
                                .methods
                                .iter()
                                .map(|(name, m)| (name.clone(), m.declaration_span)),
                        );
                    }
                }
                result.classes.push(Class {
                    span: c.span,
                    base,
                    members: c
                        .fields
                        .iter()
                        .map(|f| (f.name.clone(), f.span))
                        .chain(c.methods.iter().map(|m| (m.name.clone(), m.span)))
                        .collect(),
                    methods: c
                        .methods
                        .iter()
                        .map(|m| (m.name.clone(), m.span, m.is_static))
                        .collect(),
                    contracts,
                });
            }
            Item::Interface(i) => {
                let Some(info) = checked.lookup_interface(&i.name) else {
                    continue;
                };
                result.classes.push(Class {
                    span: i.span,
                    base: None,
                    methods: vec![],
                    contracts: vec![],
                    members: info
                        .methods
                        .iter()
                        .map(|(name, m)| (name.clone(), m.declaration_span))
                        .collect(),
                });
                for parent in &info.extends {
                    if let Some(parent) = checked.lookup_interface(parent) {
                        for method in &i.methods {
                            if let Some(base) = parent.methods.get(&method.name) {
                                result.interfaces.push((
                                    Declaration::new(&method.name, "method", method.span, None),
                                    Declaration::new(
                                        &method.name,
                                        "method",
                                        base.declaration_span,
                                        None,
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
            Item::Enum(e) => result.classes.push(Class {
                span: e.span,
                base: None,
                methods: vec![],
                contracts: vec![],
                members: e
                    .variants
                    .iter()
                    .map(|v| (v.name.clone(), v.span))
                    .collect(),
            }),
            _ => {}
        }
    }
    result
}

/// One forest traversal shares ancestor bindings across all modules and names.
/// No per-query parent-chain walks, including for inherited implementations.
fn class_relations(classes: &[&Class], mut link: impl FnMut(&str, Span, Span)) -> usize {
    let by_span: HashMap<_, _> = classes
        .iter()
        .enumerate()
        .map(|(i, c)| (c.span, i))
        .collect();
    let mut children = vec![Vec::new(); classes.len()];
    let mut pending = Vec::new();
    for (i, c) in classes.iter().enumerate() {
        if let Some(parent) = c.base.and_then(|base| by_span.get(&base)) {
            children[*parent].push(i);
        } else {
            pending.push((i, false));
        }
    }
    let mut active: HashMap<&str, Vec<(Span, bool)>> = HashMap::new();
    let mut visits = 0;
    while let Some((i, exit)) = pending.pop() {
        visits += 1;
        let c = classes[i];
        if exit {
            for (name, _, _) in &c.methods {
                visits += 1;
                active.get_mut(name.as_str()).unwrap().pop();
            }
            continue;
        }
        for (name, span, is_static) in &c.methods {
            visits += 1;
            let stack = active.entry(name).or_default();
            if !is_static && let Some(&(parent, false)) = stack.last() {
                link(name, *span, parent);
            }
            stack.push((*span, *is_static));
        }
        for (name, contract) in &c.contracts {
            visits += 1;
            if let Some(&(implementation, _)) = active.get(name.as_str()).and_then(|v| v.last()) {
                link(name, *contract, implementation);
            }
        }
        pending.push((i, true));
        pending.extend(children[i].iter().map(|&child| (child, false)));
    }
    visits
}

pub(super) fn links(
    captures: &HashMap<UnitId, CapturedUnit>,
    paths: &HashMap<UnitId, String>,
    names: &HashMap<UnitId, symbols::Names>,
    functions: &[Function],
    symbols: &[symbols::Symbol],
) -> Vec<(String, String)> {
    let by_location: HashMap<_, _> = symbols
        .iter()
        .filter_map(|s| {
            s.location
                .as_ref()
                .map(|l| ((l.path.as_str(), l.start, l.end), s.id.as_str()))
        })
        .collect();
    let mut resolved = HashMap::<Span, HashMap<String, Option<&str>>>::new();
    let mut locate = |name: &str, span: Span| {
        let names_at_span = resolved.entry(span).or_default();
        if let Some(id) = names_at_span.get(name) {
            return *id;
        }
        let id = symbols::select(span, name, false, names, paths)
            .and_then(|l| by_location.get(&(l.path.as_str(), l.start, l.end)).copied());
        names_at_span.insert(name.into(), id);
        id
    };
    let mut links = HashSet::new();
    let mut link = |name: &str, a: Span, b: Span| {
        if a == b {
            return;
        }
        if let (Some(a), Some(b)) = (locate(name, a), locate(name, b)) {
            links.insert((a.to_owned(), b.to_owned()));
        }
    };
    let classes: Vec<_> = captures
        .values()
        .flat_map(|c| &c.semantic.rename_relations.classes)
        .collect();
    class_relations(&classes, &mut link);
    for capture in captures.values() {
        for (a, b) in &capture.semantic.rename_relations.interfaces {
            link(&a.name, a.span, b.span);
        }
    }
    // Default bodies can have several compiled copies with one source declaration.
    let mut declarations = HashMap::new();
    for function in functions {
        for l in &function.locations {
            if let Some(previous) =
                declarations.insert((l.path.as_str(), l.start, l.end), function.id.as_str())
            {
                links.insert((previous.to_owned(), function.id.clone()));
            }
        }
    }
    links.into_iter().collect()
}

/// DFS intervals make each owner relationship comparison O(1).
/// Siblings and unrelated classes occupy disjoint intervals. A source method
/// can belong to several scopes (inherited contracts or injected defaults).
pub(super) fn scopes(
    captures: &HashMap<UnitId, CapturedUnit>,
    paths: &HashMap<UnitId, String>,
    names: &HashMap<UnitId, symbols::Names>,
    symbols: &[symbols::Symbol],
) -> HashMap<String, Vec<(usize, usize)>> {
    let classes: Vec<_> = captures
        .values()
        .flat_map(|c| &c.semantic.rename_relations.classes)
        .collect();
    let by_span: HashMap<_, _> = classes
        .iter()
        .enumerate()
        .map(|(i, c)| (c.span, i))
        .collect();
    let by_location: HashMap<_, _> = symbols
        .iter()
        .filter_map(|s| {
            s.location
                .as_ref()
                .map(|l| ((l.path.as_str(), l.start, l.end), s.id.as_str()))
        })
        .collect();
    let mut children = vec![vec![]; classes.len()];
    let mut pending = vec![];
    for (i, c) in classes.iter().enumerate() {
        if let Some(parent) = c.base.and_then(|b| by_span.get(&b)) {
            children[*parent].push(i);
        } else {
            pending.push((i, false));
        }
    }
    let mut clock = 0;
    let mut starts = vec![0; classes.len()];
    let mut result: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    let mut locations = HashMap::new();
    while let Some((i, exit)) = pending.pop() {
        if !exit {
            starts[i] = clock;
            clock += 1;
            pending.push((i, true));
            pending.extend(children[i].iter().map(|&c| (c, false)));
        } else {
            for (name, span) in &classes[i].members {
                let id = locations.entry((*span, name.as_str())).or_insert_with(|| {
                    symbols::select(*span, name, false, names, paths)
                        .and_then(|l| by_location.get(&(l.path.as_str(), l.start, l.end)).copied())
                });
                if let Some(id) = id {
                    result
                        .entry((*id).to_owned())
                        .or_default()
                        .push((starts[i], clock));
                }
            }
        }
    }
    result
}

type Point<'a> = (&'a str, usize, usize);
pub(super) struct Index<'a> {
    symbols: HashMap<&'a str, &'a symbols::Symbol>,
    scopes: &'a HashMap<String, Vec<(usize, usize)>>,
    destinations: HashMap<&'a str, Vec<(usize, usize, &'a str)>>,
    references: HashMap<&'a str, Vec<&'a symbols::Reference>>,
    occurrences: HashMap<Point<'a>, HashSet<&'a str>>,
    links: HashMap<&'a str, Vec<&'a str>>,
    renamed: HashSet<&'a str>,
    unknown: HashMap<String, (String, Span)>,
    source_tokens: HashMap<Point<'a>, (&'a str, &'a Span)>,
    local_ranges: HashMap<&'a str, Location>,
    named_tokens: HashMap<&'a str, Vec<(&'a str, &'a Span)>>,
    #[cfg(test)]
    indexed: usize,
    #[cfg(test)]
    edges_visited: usize,
    #[cfg(test)]
    collision_probes: usize,
}
impl<'a> Index<'a> {
    pub fn new(
        snapshot: &'a Snapshot,
        root: &Path,
        identifiers: &BTreeMap<&'a str, Vec<(&str, &'a Span)>>,
        qualifiers: &HashSet<(&str, usize, usize)>,
    ) -> Self {
        let mut result = Self {
            symbols: HashMap::new(),
            scopes: &snapshot.semantic.rename_scopes,
            destinations: HashMap::new(),
            references: HashMap::new(),
            occurrences: HashMap::new(),
            links: HashMap::new(),
            renamed: HashSet::new(),
            unknown: HashMap::new(),
            source_tokens: HashMap::new(),
            local_ranges: HashMap::new(),
            named_tokens: HashMap::new(),
            #[cfg(test)]
            indexed: 0,
            #[cfg(test)]
            edges_visited: 0,
            #[cfg(test)]
            collision_probes: 0,
        };
        let locals: Vec<_> = snapshot
            .semantic
            .symbols
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "binding" | "parameter") && s.location.is_some())
            .collect();
        let ranges: Vec<_> = snapshot
            .functions
            .iter()
            .flat_map(|f| f.locations.iter().map(|l| (l.clone(), f.id.clone())))
            .collect();
        let by_owner: HashMap<_, _> = ranges
            .iter()
            .map(|(l, id)| ((l.path.as_str(), id.as_str()), l))
            .collect();
        let points: Vec<_> = locals.iter().map(|s| s.location.clone().unwrap()).collect();
        for (symbol, owner) in locals
            .into_iter()
            .zip(crate::compiler_db::references::owners(&points, &ranges))
        {
            let (path, id): (String, String) =
                serde_json::from_str(&owner).expect("compiler owner");
            if let Some(range) = by_owner.get(&(path.as_str(), id.as_str())) {
                result.local_ranges.insert(&symbol.id, (*range).clone());
            }
        }
        let scope_count = result
            .scopes
            .values()
            .flatten()
            .map(|s| s.0)
            .max()
            .map_or(0, |n| n + 1);
        let mut by_start = vec![Vec::new(); scope_count];
        for symbol in &snapshot.semantic.symbols {
            result.symbols.insert(&symbol.id, symbol);
            for &(start, end) in result.scopes.get(&symbol.id).into_iter().flatten() {
                by_start[start].push((
                    symbol.name.rsplit("::").next().unwrap(),
                    end,
                    symbol.id.as_str(),
                ));
            }
            if let Some(l) = &symbol.location {
                result
                    .occurrences
                    .entry((&l.path, l.start, l.end))
                    .or_default()
                    .insert(&symbol.id);
            }
        }
        // DFS starts are dense integer keys: bucket once rather than sorting
        // every spelling. Prefix maxima detect overlaps with one binary search.
        for (start, members) in by_start.into_iter().enumerate() {
            for (name, end, id) in members {
                let candidates = result.destinations.entry(name).or_default();
                let (_, widest, witness) = candidates.last().copied().unwrap_or((0, 0, id));
                candidates.push(if end > widest {
                    (start, end, id)
                } else {
                    (start, widest, witness)
                });
            }
        }
        for reference in &snapshot.semantic.references {
            result
                .references
                .entry(&reference.target)
                .or_default()
                .push(reference);
            let l = &reference.location;
            result
                .occurrences
                .entry((&l.path, l.start, l.end))
                .or_default()
                .insert(&reference.target);
        }
        for (a, b) in &snapshot.semantic.rename_links {
            result.links.entry(a).or_default().push(b);
            result.links.entry(b).or_default().push(a);
        }
        let paths: HashMap<_, _> = snapshot
            .sources
            .keys()
            .filter_map(|absolute| {
                Some((
                    Path::new(absolute)
                        .strip_prefix(root)
                        .ok()?
                        .to_str()?
                        .to_owned(),
                    absolute.as_str(),
                ))
            })
            .collect();
        for (&name, positions) in identifiers {
            for &(path, span) in positions {
                #[cfg(test)]
                {
                    result.indexed += 1;
                }
                if let Some(&absolute) = paths.get(path) {
                    result
                        .named_tokens
                        .entry(name)
                        .or_default()
                        .push((absolute, span));
                    result
                        .source_tokens
                        .insert((absolute, span.start, span.end), (name, span));
                }
                if !qualifiers.contains(&(path, span.start, span.end))
                    && !result.unknown.contains_key(name)
                    && paths.get(path).is_none_or(|absolute| {
                        !result
                            .occurrences
                            .contains_key(&(*absolute, span.start, span.end))
                    })
                {
                    result.unknown.insert(name.into(), (path.into(), *span));
                }
            }
        }
        // edit::structured_changes supplies identifiers in BTreeMap file order,
        // then lexer offset order; canonical editable paths share the root prefix.
        // Preserve that order instead of sorting all tokens again.
        debug_assert!(
            result
                .named_tokens
                .values()
                .all(|tokens| tokens
                    .windows(2)
                    .all(|pair| (pair[0].0, pair[0].1.start) <= (pair[1].0, pair[1].1.start)))
        );
        result
    }
    pub fn supports(&self, id: &str) -> bool {
        self.symbols.get(id).is_some_and(|s| {
            matches!(
                s.kind.as_str(),
                "method" | "field" | "static-field" | "variant" | "binding" | "parameter"
            )
        })
    }
    pub fn plan(
        &mut self,
        id: &str,
        name: &str,
        root: &Path,
        patches: &mut BTreeMap<String, Vec<Patch>>,
        work: &mut EditWork,
    ) -> Result<()> {
        let selected = self.symbols[id];
        let old = selected.name.rsplit("::").next().unwrap();
        validate_name(name)?;
        ensure!(name != old, "rename has no effect");
        ensure!(
            !self.renamed.contains(id),
            "duplicate rename of dispatch family"
        );
        let local = matches!(selected.kind.as_str(), "binding" | "parameter");
        if local {
            let range = self
                .local_ranges
                .get(id)
                .context("local binding has no editable function scope")?;
            // Conservative capture prevention: any existing destination spelling
            // in the enclosing function (including nested closures) is rejected.
            // Indexed range lookup avoids a token scan for each local rename.
            if let Some(tokens) = self.named_tokens.get(name) {
                let i = tokens.partition_point(|(path, span)| {
                    #[cfg(test)]
                    {
                        self.collision_probes += 1;
                    }
                    (*path, span.start) < (range.path.as_str(), range.start)
                });
                if let Some(&(path, span)) = tokens.get(i)
                    && path == range.path
                    && span.start < range.end
                {
                    let relative = Path::new(path)
                        .strip_prefix(root)?
                        .to_str()
                        .context("non UTF-8 path")?;
                    return Err(reject(
                        "rename destination already occurs in the enclosing function; renaming could capture or shadow another binding",
                        relative,
                        span,
                    ));
                }
            }
        }
        if !local && let Some((path, span)) = self.unknown.get(old) {
            return Err(reject(
                "rename coverage incomplete: identifier has no proven semantic target",
                path,
                span,
            ));
        }
        let mut family = HashSet::new();
        let mut pending = vec![selected.id.as_str()];
        while let Some(id) = pending.pop() {
            if family.insert(id)
                && let Some(edges) = self.links.get(id)
            {
                #[cfg(test)]
                {
                    self.edges_visited += edges.len();
                }
                pending.extend(edges);
            }
        }
        ensure!(
            family.is_disjoint(&self.renamed),
            "duplicate rename of dispatch family"
        );
        for &(start, end) in family
            .iter()
            .filter_map(|id| self.scopes.get(*id))
            .flatten()
        {
            let Some(candidates) = self.destinations.get(name) else {
                continue;
            };
            let before = candidates.partition_point(|&(other_start, _, _)| {
                #[cfg(test)]
                {
                    self.collision_probes += 1;
                }
                other_start < end
            });
            if let Some(&(_, other_end, candidate)) = before.checked_sub(1).map(|i| &candidates[i])
                && other_end > start
            {
                let l = self.symbols[candidate].location.as_ref().unwrap();
                let (_, span) = self
                    .source_tokens
                    .get(&(l.path.as_str(), l.start, l.end))
                    .context("collision declaration is not an editable identifier")?;
                let path = Path::new(&l.path)
                    .strip_prefix(root)?
                    .to_str()
                    .context("non UTF-8 source path")?;
                return Err(reject(
                    "rename destination conflicts with a member in the same hierarchy",
                    path,
                    span,
                ));
            }
        }
        self.renamed.extend(&family);
        let mut allowed = HashSet::new();
        for id in &family {
            if let Some(symbol) = self.symbols.get(id) {
                let l = symbol
                    .location
                    .as_ref()
                    .context("rename declaration is outside editable source")?;
                if allowed.insert((l.path.as_str(), l.start, l.end)) {
                    work.declarations += 1;
                }
            }
        }
        // Count unique declaration tokens before references: copied/default
        // methods may have several semantic identities at one source location.
        for id in &family {
            for reference in self.references.get(id).into_iter().flatten() {
                work.references_visited += 1;
                let l = &reference.location;
                allowed.insert((l.path.as_str(), l.start, l.end));
            }
        }
        // Every declaration/reference must map to an editable token with this spelling.
        for (absolute, start, end) in allowed {
            let (spelling, span) = self
                .source_tokens
                .get(&(absolute, start, end))
                .context("rename reference is not an exact editable source identifier")?;
            ensure!(
                *spelling == old,
                "rename reference has a different source spelling"
            );
            let path = Path::new(absolute)
                .strip_prefix(root)?
                .to_str()
                .context("non UTF-8 source path")?;
            if self
                .occurrences
                .get(&(absolute, start, end))
                .is_some_and(|ids| ids.iter().any(|id| !family.contains(id)))
            {
                return Err(reject(
                    "rename occurrence has conflicting semantic targets",
                    path,
                    span,
                ));
            }
            patches.entry(path.into()).or_default().push(Patch {
                start,
                end,
                text: name.into(),
            });
        }
        Ok(())
    }
}
fn reject(message: &str, path: &str, span: &Span) -> anyhow::Error {
    Rejection {
        message: message.into(),
        location: RejectionLocation {
            path: path.into(),
            start: span.start,
            end: span.end,
            line: span.line,
            column: span.col,
        },
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hierarchy_relations_share_deep_chains_and_distinct_method_queries() {
        for n in [16, 64, 256, 1024] {
            for shape in ["chain", "fanout"] {
                let span = |i| Span::new(i * 10, i * 10 + 1, 1, 1);
                let classes: Vec<_> = (0..n)
                    .map(|i| Class {
                        span: span(i),
                        members: vec![],
                        base: (i > 0).then(|| span(if shape == "chain" { i - 1 } else { 0 })),
                        methods: if i == 0 {
                            (0..n)
                                .map(|j| (format!("m{j}"), span(n + j), false))
                                .collect()
                        } else {
                            vec![]
                        },
                        contracts: if i > 0 {
                            vec![(format!("m{i}"), span(2 * n + i))]
                        } else {
                            vec![]
                        },
                    })
                    .collect();
                let mut links = Vec::new();
                let visits = class_relations(&classes.iter().collect::<Vec<_>>(), |name, a, b| {
                    links.push((name.to_owned(), a, b))
                });
                assert_eq!(visits, 5 * n - 1);
                assert_eq!(links.len(), n - 1);
                for (name, _, implementation) in &links {
                    let i = name[1..].parse::<usize>().unwrap();
                    assert_eq!(*implementation, span(n + i));
                }
                println!(
                    "hierarchy={shape} classes={n} distinct_methods={n} visits={visits} links={}",
                    links.len()
                );
            }
        }
    }

    #[test]
    fn unrelated_destination_queries_do_not_multiply_full_scans() {
        for kind in ["field", "binding"] {
            for n in [16usize, 64, 256, 1024] {
                let root = std::env::temp_dir().join("willow-collision-index");
                let path = root.join("main.wi").to_str().unwrap().to_owned();
                let tokens = Lexer::new(&"old next ".repeat(n)).tokenize().unwrap();
                let mut identifiers = BTreeMap::<&str, Vec<(&str, &Span)>>::new();
                let mut symbols = vec![];
                let mut scopes = HashMap::new();
                for (i, t) in tokens.iter().enumerate().take(2 * n) {
                    let TokenKind::Ident(name) = &t.kind else {
                        panic!()
                    };
                    identifiers
                        .entry(name)
                        .or_default()
                        .push(("main.wi", &t.span));
                    let id = format!("s{i}");
                    scopes.insert(id.clone(), vec![(i, i + 1)]);
                    symbols.push(symbols::Symbol {
                        id,
                        identity: None,
                        source_name: None,
                        name: name.clone(),
                        kind: kind.into(),
                        ty: None,
                        location: Some(Location {
                            path: path.clone(),
                            start: t.span.start,
                            end: t.span.end,
                        }),
                    });
                }
                let functions = if kind == "binding" {
                    symbols.iter().map(|s| serde_json::from_value(serde_json::json!({
                    "id":format!("fn{}", s.id), "module":"main", "name":format!("fn{}", s.id),
                    "locations":[s.location], "synthetic":false, "fingerprint":"", "body_fingerprint":"",
                    "callees":[], "runtime_effects":0, "unknown":false, "unresolved":[]
                })).unwrap()).collect()
                } else {
                    vec![]
                };
                let snapshot = Snapshot {
                    edit_context: None,
                    version: 1,
                    compiler: String::new(),
                    compatibility: String::new(),
                    workspace: root.to_str().unwrap().into(),
                    revision: String::new(),
                    functions,
                    sources: BTreeMap::from([(path, String::new())]),
                    semantic: SemanticFacts {
                        symbols,
                        rename_scopes: if kind == "field" {
                            scopes
                        } else {
                            HashMap::new()
                        },
                        ..Default::default()
                    },
                };
                let mut index = Index::new(&snapshot, &root, &identifiers, &HashSet::new());
                let mut patches = BTreeMap::new();
                let mut work = EditWork::default();
                for i in 0..n {
                    index
                        .plan(
                            &format!("s{}", 2 * i),
                            "next",
                            &root,
                            &mut patches,
                            &mut work,
                        )
                        .unwrap();
                }
                assert_eq!(index.indexed, 2 * n);
                assert_eq!(patches["main.wi"].len(), n);
                assert!(index.collision_probes <= n * (n.ilog2() as usize + 2));
                println!(
                    "kind={kind} destinations={n} queries={n} indexed={} collision_probes={}",
                    index.indexed, index.collision_probes
                );
            }
        }
    }

    #[test]
    fn indexed_members_scale_with_occurrences_and_family_edges() {
        for shape in ["chain", "fanout", "disjoint", "shared"] {
            for n in [16, 64, 256, 1024] {
                let root = std::env::temp_dir().join("willow-rename-index");
                let absolute = root.join("main.wi").to_str().unwrap().to_owned();
                let source = if shape == "shared" {
                    "old ".repeat(n + 1)
                } else {
                    "old old ".repeat(n)
                };
                let tokens = Lexer::new(&source).tokenize().unwrap();
                let spans: Vec<_> = tokens
                    .iter()
                    .filter(|t| matches!(t.kind, TokenKind::Ident(_)))
                    .map(|t| &t.span)
                    .collect();
                let symbols: Vec<_> = (0..n)
                    .map(|i| symbols::Symbol {
                        id: format!("s{i}"),
                        identity: None,
                        source_name: None,
                        name: "old".into(),
                        kind: "method".into(),
                        ty: None,
                        location: Some(Location {
                            path: absolute.clone(),
                            start: spans[if shape == "shared" { 0 } else { 2 * i }].start,
                            end: spans[if shape == "shared" { 0 } else { 2 * i }].end,
                        }),
                    })
                    .collect();
                let references = (0..n)
                    .map(|i| symbols::Reference {
                        target: format!("s{i}"),
                        identity: None,
                        role: "member".into(),
                        location: Location {
                            path: absolute.clone(),
                            start: spans[if shape == "shared" { i + 1 } else { 2 * i + 1 }].start,
                            end: spans[if shape == "shared" { i + 1 } else { 2 * i + 1 }].end,
                        },
                    })
                    .collect();
                let rename_links = if shape == "disjoint" {
                    vec![]
                } else {
                    (1..n)
                        .map(|i| {
                            (
                                format!("s{}", if shape == "chain" { i - 1 } else { 0 }),
                                format!("s{i}"),
                            )
                        })
                        .collect()
                };
                let snapshot = Snapshot {
                    edit_context: None,
                    version: 1,
                    compiler: String::new(),
                    compatibility: String::new(),
                    workspace: root.to_str().unwrap().into(),
                    revision: String::new(),
                    functions: vec![],
                    sources: BTreeMap::from([(absolute, String::new())]),
                    semantic: SemanticFacts {
                        symbols,
                        references,
                        rename_links,
                        ..Default::default()
                    },
                };
                let identifiers =
                    BTreeMap::from([("old", spans.iter().map(|&s| ("main.wi", s)).collect())]);
                let mut index = Index::new(&snapshot, &root, &identifiers, &HashSet::new());
                let mut work = EditWork::default();
                let mut patches = BTreeMap::new();
                for i in 0..if shape == "disjoint" { n } else { 1 } {
                    index
                        .plan(
                            &format!("s{i}"),
                            &format!("new{i}"),
                            &root,
                            &mut patches,
                            &mut work,
                        )
                        .unwrap();
                }
                assert_eq!(index.indexed, if shape == "shared" { n + 1 } else { 2 * n });
                assert_eq!(work.references_visited, n);
                assert_eq!(work.declarations, if shape == "shared" { 1 } else { n });
                assert_eq!(
                    patches["main.wi"].len(),
                    if shape == "shared" { n + 1 } else { 2 * n }
                );
                assert_eq!(
                    index.edges_visited,
                    if shape == "disjoint" { 0 } else { 2 * (n - 1) }
                );
                assert!(
                    index
                        .plan("s0", "again", &root, &mut patches, &mut work)
                        .is_err()
                );
                println!(
                    "shape={shape} n={n} indexed={} refs={} edges={} patches={}",
                    index.indexed,
                    work.references_visited,
                    index.edges_visited,
                    patches["main.wi"].len()
                );
            }
        }
    }
}
