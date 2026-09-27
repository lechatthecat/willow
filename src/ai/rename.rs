//! Live semantic rename relationships and indexed source patches.
use super::*;
use crate::lexer::{Lexer, token::TokenKind};
use crate::semantic::{analysis_symbols::Declaration, symbols::SymbolTable};
use edit::{EditWork, Patch, Rejection, RejectionLocation};
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct Relations {
    classes: Vec<Class>,
    interfaces: Vec<(Declaration, Declaration)>,
}
struct Class {
    span: Span,
    base: Option<Span>,
    methods: Vec<(String, Span, bool)>,
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

type Point<'a> = (&'a str, usize, usize);
pub(super) struct Index<'a> {
    symbols: HashMap<&'a str, &'a symbols::Symbol>,
    references: HashMap<&'a str, Vec<&'a symbols::Reference>>,
    occurrences: HashMap<Point<'a>, HashSet<&'a str>>,
    links: HashMap<&'a str, Vec<&'a str>>,
    renamed: HashSet<&'a str>,
    unknown: HashMap<String, (String, Span)>,
    source_tokens: HashMap<Point<'a>, (&'a str, &'a Span)>,
    #[cfg(test)]
    indexed: usize,
    #[cfg(test)]
    edges_visited: usize,
}
impl<'a> Index<'a> {
    pub fn new(
        snapshot: &'a Snapshot,
        root: &Path,
        identifiers: &BTreeMap<&'a str, Vec<(&str, &'a Span)>>,
    ) -> Self {
        let mut result = Self {
            symbols: HashMap::new(),
            references: HashMap::new(),
            occurrences: HashMap::new(),
            links: HashMap::new(),
            renamed: HashSet::new(),
            unknown: HashMap::new(),
            source_tokens: HashMap::new(),
            #[cfg(test)]
            indexed: 0,
            #[cfg(test)]
            edges_visited: 0,
        };
        for symbol in &snapshot.semantic.symbols {
            result.symbols.insert(&symbol.id, symbol);
            if let Some(l) = &symbol.location {
                result
                    .occurrences
                    .entry((&l.path, l.start, l.end))
                    .or_default()
                    .insert(&symbol.id);
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
                        .source_tokens
                        .insert((absolute, span.start, span.end), (name, span));
                }
                if !result.unknown.contains_key(name)
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
        result
    }
    pub fn supports(&self, id: &str) -> bool {
        self.symbols.get(id).is_some_and(|s| {
            matches!(
                s.kind.as_str(),
                "method" | "field" | "static-field" | "variant"
            )
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub fn plan(
        &mut self,
        id: &str,
        name: &str,
        root: &Path,
        identifiers: &BTreeMap<&str, Vec<(&str, &Span)>>,
        patches: &mut BTreeMap<String, Vec<Patch>>,
        work: &mut EditWork,
    ) -> Result<()> {
        let selected = self.symbols[id];
        let old = selected.name.rsplit("::").next().unwrap();
        let ts = Lexer::new(name)
            .tokenize()
            .map_err(|_| anyhow::anyhow!("invalid identifier"))?;
        ensure!(
            ts.len() == 2 && matches!(&ts[0].kind, TokenKind::Ident(s) if s == name),
            "rename requires an identifier"
        );
        ensure!(name != old, "rename has no effect");
        if let Some(&(path, span)) = identifiers.get(name).and_then(|v| v.first()) {
            return Err(reject(
                "rename destination already occurs in workspace",
                path,
                span,
            ));
        }
        ensure!(
            !self.renamed.contains(id),
            "duplicate rename of dispatch family"
        );
        if let Some((path, span)) = self.unknown.get(old) {
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
        self.renamed.extend(&family);
        let mut allowed = HashSet::new();
        for id in &family {
            if let Some(symbol) = self.symbols.get(id) {
                let l = symbol
                    .location
                    .as_ref()
                    .context("rename declaration is outside editable source")?;
                allowed.insert((l.path.as_str(), l.start, l.end));
                work.declarations += 1;
            }
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
    fn indexed_members_scale_with_occurrences_and_family_edges() {
        for shape in ["chain", "fanout", "disjoint"] {
            for n in [16, 64, 256, 1024] {
                let root = std::env::temp_dir().join("willow-rename-index");
                let absolute = root.join("main.wi").to_str().unwrap().to_owned();
                let source = "old old ".repeat(n);
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
                            start: spans[2 * i].start,
                            end: spans[2 * i].end,
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
                            start: spans[2 * i + 1].start,
                            end: spans[2 * i + 1].end,
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
                let mut index = Index::new(&snapshot, &root, &identifiers);
                let mut work = EditWork::default();
                let mut patches = BTreeMap::new();
                for i in 0..if shape == "disjoint" { n } else { 1 } {
                    index
                        .plan(
                            &format!("s{i}"),
                            &format!("new{i}"),
                            &root,
                            &identifiers,
                            &mut patches,
                            &mut work,
                        )
                        .unwrap();
                }
                assert_eq!(index.indexed, 2 * n);
                assert_eq!(work.references_visited, n);
                assert_eq!(work.declarations, n);
                assert_eq!(patches["main.wi"].len(), 2 * n);
                assert_eq!(
                    index.edges_visited,
                    if shape == "disjoint" { 0 } else { 2 * (n - 1) }
                );
                assert!(
                    index
                        .plan("s0", "again", &root, &identifiers, &mut patches, &mut work)
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
