//! Bound production Rust source size, including inactive platform modules.
//! Only modules proven to require `cfg(test)` are exempt; names are irrelevant.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const LIMIT: usize = 3000;
// Exceptional production files must have a specific architectural reason.
// Entries that no longer need an exception fail the guard too.
const ALLOWLIST: &[(&str, &str)] = &[];

fn size_violation(path: &str, lines: usize) -> Option<String> {
    (lines > LIMIT).then(|| format!("{path}: {lines} lines (limit {LIMIT})"))
}

fn requires_test(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) => {
            let parts = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .expect("valid cfg predicate");
            if list.path.is_ident("all") {
                parts.iter().any(requires_test)
            } else if list.path.is_ident("any") {
                !parts.is_empty() && parts.iter().all(requires_test)
            } else {
                false
            }
        }
        syn::Meta::NameValue(_) => false,
    }
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && requires_test(&attr.parse_args::<syn::Meta>().expect("valid cfg"))
    })
}

fn collect_rs(directory: &Path, files: &mut HashSet<PathBuf>) {
    for entry in std::fs::read_dir(directory).expect("read source directory") {
        let path = entry.expect("source entry").path();
        if path.is_dir() {
            collect_rs(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.insert(path.canonicalize().expect("source path"));
        }
    }
}

type Edges = Vec<(PathBuf, bool)>;

fn module_edges(
    items: &[syn::Item],
    directory: &Path,
    path_directory: &Path,
    gated: bool,
    edges: &mut Edges,
) {
    for item in items {
        let syn::Item::Mod(module) = item else {
            continue;
        };
        let gated = gated || test_only(&module.attrs);
        let explicit = module.attrs.iter().find_map(|attr| {
            if !attr.path().is_ident("path") {
                return None;
            }
            let syn::Meta::NameValue(value) = &attr.meta else {
                panic!("path must name a file")
            };
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(path),
                ..
            }) = &value.value
            else {
                panic!("path must be a string")
            };
            Some(path_directory.join(path.value()))
        });
        let base = explicit.unwrap_or_else(|| directory.join(module.ident.to_string()));
        if let Some((_, children)) = &module.content {
            module_edges(children, &base, &base, gated, edges);
        } else {
            let path = if base.extension().is_some() {
                base
            } else if base.with_extension("rs").is_file() {
                base.with_extension("rs")
            } else {
                base.join("mod.rs")
            };
            edges.push((path.canonicalize().expect("declared module exists"), gated));
        }
    }
}

fn production_files(
    graph: &HashMap<PathBuf, Edges>,
    roots: &HashSet<PathBuf>,
) -> (HashSet<PathBuf>, usize) {
    // First find test-only subtrees, including their ordinary child modules.
    let mut tests = HashSet::new();
    let mut edge_visits = 0;
    let mut pending: Vec<_> = graph
        .values()
        .flatten()
        .inspect(|_| edge_visits += 1)
        .filter(|(_, gated)| *gated)
        .map(|(path, _)| path.clone())
        .collect();
    while let Some(path) = pending.pop() {
        if tests.insert(path.clone()) {
            pending.extend(
                graph
                    .get(&path)
                    .into_iter()
                    .flatten()
                    .inspect(|_| edge_visits += 1)
                    .map(|(p, _)| p.clone()),
            );
        }
    }
    // Shared files also reachable from production remain production. Unreferenced
    // files are checked conservatively rather than silently escaping the limit.
    let mut production = HashSet::new();
    pending.extend(roots.iter().cloned());
    pending.extend(graph.keys().filter(|path| !tests.contains(*path)).cloned());
    while let Some(path) = pending.pop() {
        if production.insert(path.clone()) {
            pending.extend(
                graph
                    .get(&path)
                    .into_iter()
                    .flatten()
                    .inspect(|_| edge_visits += 1)
                    .filter(|(_, gated)| !*gated)
                    .map(|(path, _)| path.clone()),
            );
        }
    }
    (production, edge_visits)
}

fn add_root(package: &Path, relative: &str, roots: &mut HashSet<PathBuf>) {
    let path = package.join(relative);
    if path.is_file() {
        roots.insert(path.canonicalize().unwrap());
    }
}

#[test]
fn production_rust_files_stay_within_limit() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap();
    let manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(root.join("Cargo.toml")).unwrap()).unwrap();
    let mut files = HashSet::new();
    let mut roots = HashSet::new();
    for member in manifest["workspace"]["members"].as_array().unwrap() {
        let package = root.join(member.as_str().unwrap());
        collect_rs(&package.join("src"), &mut files);
        let package_manifest: toml::Value =
            toml::from_str(&std::fs::read_to_string(package.join("Cargo.toml")).unwrap()).unwrap();
        for default in ["src/lib.rs", "src/main.rs", "build.rs"] {
            add_root(&package, default, &mut roots);
        }
        if let Some(path) = package_manifest.get("lib").and_then(|lib| lib.get("path")) {
            add_root(&package, path.as_str().unwrap(), &mut roots);
        }
        if let Some(bins) = package_manifest.get("bin").and_then(toml::Value::as_array) {
            for bin in bins {
                if let Some(path) = bin.get("path") {
                    add_root(&package, path.as_str().unwrap(), &mut roots);
                }
            }
        }
        if let Some(path) = package_manifest["package"]
            .get("build")
            .and_then(toml::Value::as_str)
        {
            add_root(&package, path, &mut roots);
        }
    }
    for file in &files {
        let parent = file.parent().unwrap();
        if parent.ends_with("src/bin")
            || (file.ends_with("main.rs")
                && parent.parent().is_some_and(|p| p.ends_with("src/bin")))
        {
            roots.insert(file.clone());
        }
    }
    files.extend(roots.iter().cloned());
    let mut graph = HashMap::new();
    let mut lines = HashMap::new();
    for file in &files {
        let source = std::fs::read_to_string(file).unwrap();
        lines.insert(file.clone(), source.lines().count());
        let syntax = syn::parse_file(&source).unwrap_or_else(|error| {
            panic!("{}: {error}", file.display());
        });
        let parent = file.parent().unwrap();
        let directory = if roots.contains(file) || file.ends_with("mod.rs") {
            parent.to_path_buf()
        } else {
            file.with_extension("")
        };
        let mut edges = Vec::new();
        module_edges(&syntax.items, &directory, parent, false, &mut edges);
        graph.insert(file.clone(), edges);
    }
    let mut exceptions: HashMap<_, _> = ALLOWLIST.iter().copied().collect();
    assert_eq!(exceptions.len(), ALLOWLIST.len(), "duplicate exception");
    let mut violations = Vec::new();
    for file in production_files(&graph, &roots).0 {
        let count = lines[&file];
        let relative = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if let Some(reason) = exceptions.remove(relative.as_str()) {
            assert!(!reason.trim().is_empty(), "{relative}: missing reason");
            assert!(count > LIMIT, "{relative}: remove stale exception");
        } else if let Some(violation) = size_violation(&relative, count) {
            violations.push(violation);
        }
    }
    assert!(
        exceptions.is_empty(),
        "unknown/non-production exceptions: {exceptions:?}"
    );
    violations.sort();
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn only_cfg_predicates_that_require_tests_are_exempt() {
    for predicate in ["test", "all(test, unix)", "any(test, all(test, windows))"] {
        assert!(
            requires_test(&syn::parse_str(predicate).unwrap()),
            "{predicate}"
        );
    }
    for predicate in ["unix", "not(test)", "any(test, unix)", "all()", "any()"] {
        assert!(
            !requires_test(&syn::parse_str(predicate).unwrap()),
            "{predicate}"
        );
    }
}

#[test]
fn shared_files_and_orphans_remain_production() {
    let graph = HashMap::from([
        (
            PathBuf::from("lib"),
            vec![("tests".into(), true), ("shared".into(), false)],
        ),
        (
            PathBuf::from("tests"),
            vec![("helper".into(), false), ("shared".into(), false)],
        ),
        (PathBuf::from("helper"), vec![]),
        (PathBuf::from("shared"), vec![]),
        (PathBuf::from("orphan"), vec![]),
    ]);
    assert_eq!(
        production_files(&graph, &HashSet::from([PathBuf::from("lib")])).0,
        ["lib", "shared", "orphan"].map(PathBuf::from).into()
    );
}

#[test]
fn crate_roots_remain_production_even_when_a_test_imports_them() {
    let graph = HashMap::from([
        (PathBuf::from("lib"), vec![("tests".into(), true)]),
        (PathBuf::from("tests"), vec![("lib".into(), false)]),
    ]);
    assert_eq!(
        production_files(&graph, &HashSet::from([PathBuf::from("lib")])).0,
        HashSet::from([PathBuf::from("lib")])
    );
}

#[test]
fn raw_line_limit_includes_comments_and_the_final_unterminated_line() {
    for newline in ["\n", "\r\n"] {
        let source = format!("// counted{newline}").repeat(LIMIT);
        assert!(size_violation("source.rs", source.lines().count()).is_none());
        let oversized = source + "// final line";
        assert_eq!(
            size_violation("source.rs", oversized.lines().count()).as_deref(),
            Some("source.rs: 3001 lines (limit 3000)")
        );
    }
    assert!(size_violation("empty.rs", "".lines().count()).is_none());
}

#[test]
fn graph_walk_is_linear_for_deep_chains_and_repeated_shared_fanout() {
    for count in [16, 64, 256, 1024] {
        let nodes: Vec<PathBuf> = (0..count).map(|i| format!("node{i}").into()).collect();
        let mut graph = HashMap::from([
            (
                PathBuf::from("lib"),
                vec![(nodes[0].clone(), false), ("tests".into(), true)],
            ),
            (
                PathBuf::from("tests"),
                nodes
                    .iter()
                    .cycle()
                    .take(8 * count)
                    .map(|p| (p.clone(), false))
                    .collect(),
            ),
        ]);
        for (i, node) in nodes.iter().enumerate() {
            graph.insert(
                node.clone(),
                nodes
                    .get(i + 1)
                    .map(|p| vec![(p.clone(), false)])
                    .unwrap_or_default(),
            );
        }
        let edges: usize = graph.values().map(Vec::len).sum();
        let (production, visits) = production_files(&graph, &HashSet::from([PathBuf::from("lib")]));
        assert_eq!(production.len(), count + 1);
        assert!(!production.contains(Path::new("tests")));
        assert!(
            visits <= 3 * edges,
            "{visits} edge visits for {edges} edges"
        );
        println!("module graph: chain={count}, edges={edges}, edge_visits={visits}");
    }
}
