//! Conservative, scope-local structural correspondence for verification reports.
use super::*;
use crate::qualified_name::{QualifiedName, SegmentRun};
use std::sync::LazyLock;

/// `::function::<lambda `: a lambda's key.
static LAMBDA_RUN: LazyLock<SegmentRun> =
    LazyLock::new(|| SegmentRun::new(["function"], "<lambda "));
/// `::function::$dispatch$`: a dispatch summary's key.
static DISPATCH_RUN: LazyLock<SegmentRun> =
    LazyLock::new(|| SegmentRun::new(["function"], "$dispatch$"));
/// `::function::`: a free function's key.
static FUNCTION_RUN: LazyLock<SegmentRun> = LazyLock::new(|| SegmentRun::new(["function"], ""));
/// `::method::`: a method's key.
static METHOD_RUN: LazyLock<SegmentRun> = LazyLock::new(|| SegmentRun::new(["method"], ""));

pub(super) fn update_shapes(
    shapes: &mut im::OrdMap<String, String>,
    lambda_owners: &mut im::OrdMap<String, im::OrdSet<String>>,
    snapshot: &Snapshot,
    ids: &im::OrdMap<String, String>,
) -> Result<()> {
    // One token prefix table per changed source, shared by all nested bodies.
    let mut ranges: BTreeMap<&str, Vec<(&str, &Location)>> = BTreeMap::new();
    for function in &snapshot.functions {
        let key = &ids[&function.id];
        if is_lambda(key)
            && let Some(owner) = scope(key)
        {
            lambda_owners.entry(owner).or_default().insert(key.clone());
        }
        if let Some(location) = &function.body_location {
            ranges
                .entry(&location.path)
                .or_default()
                .push((&function.id, location));
        }
    }
    for (path, ranges) in ranges {
        if super::super::super::bundled_source(path).is_some() {
            continue;
        }
        let Some(digest) = snapshot.sources.get(path) else {
            continue;
        };
        let source = std::fs::read_to_string(path)?;
        ensure!(
            hash(&source) == *digest,
            "source changed while matching declarations"
        );
        let tokens = super::super::super::TokenHashes::with_identifier_shape(&source, true)?;
        for (id, location) in ranges {
            shapes.insert(
                ids[id].clone(),
                tokens.range(crate::diagnostics::Span::new(
                    location.start,
                    location.end,
                    1,
                    1,
                )),
            );
        }
    }
    Ok(())
}

pub(super) fn scope(key: &str) -> Option<String> {
    let (module, rest) = QualifiedName::new(key).split_first()?;
    let (kind, name) = QualifiedName::new(rest).split_first()?;
    if let Some(lambda) = name.strip_prefix("<lambda ") {
        let (owner, _) = lambda.rsplit_once('@')?;
        return Some(QualifiedName::from_segments([
            module, kind, "lambda", owner,
        ]));
    }
    if name.starts_with("<function-value candidates:") {
        // Synthetic candidate identities include compiler-local type IDs.
        // Only pair a unique candidate set; its edges are still compared.
        return Some(QualifiedName::from_segments([
            module,
            kind,
            "function-value",
        ]));
    }
    let owner = QualifiedName::new(name)
        .split_last()
        .map_or("", |(owner, _)| owner);
    Some(QualifiedName::from_segments([module, kind, owner]))
}

pub(super) fn is_lambda(key: &str) -> bool {
    QualifiedName::new(key).contains_run(&LAMBDA_RUN)
}

type Pairs = BTreeMap<String, String>;
type Groups = BTreeMap<String, (Vec<String>, Vec<String>)>;

pub(super) fn pairs(
    before: &Capture,
    after: &Capture,
    keys: &BTreeSet<String>,
) -> Result<(Pairs, BTreeSet<String>)> {
    pairs_with_work(before, after, keys).map(|(pairs, keys, _)| (pairs, keys))
}

fn pair_groups(groups: Groups, occurrences: bool) -> Pairs {
    let mut result = BTreeMap::new();
    for (mut old, mut new) in groups.into_values() {
        if old.len() == 1 && new.len() == 1 {
            result.insert(old.remove(0), new.remove(0));
        } else if occurrences && !old.is_empty() && old.len() == new.len() {
            let offset = |key: &String| {
                key.rsplit_once('@')
                    .and_then(|(_, suffix)| suffix.split_once(':'))
                    .and_then(|(start, _)| start.parse::<usize>().ok())
                    .unwrap_or(0)
            };
            old.sort_by_key(offset);
            new.sort_by_key(offset);
            result.extend(old.into_iter().zip(new));
        }
    }
    result
}

fn pairs_with_work(
    before: &Capture,
    after: &Capture,
    keys: &BTreeSet<String>,
) -> Result<(Pairs, BTreeSet<String>, usize)> {
    let mut visits = 0;
    let mut groups = Groups::new();
    let mut owners = BTreeSet::new();
    for (side, capture, other) in [(0, before, after), (1, after, before)] {
        for key in keys {
            visits += 1;
            if is_lambda(key) {
                if let Some(owner) = scope(key) {
                    owners.insert(owner);
                }
                continue;
            }
            if other.baseline.facts.contains_key(key)
                || QualifiedName::new(key).contains_run(&DISPATCH_RUN)
            {
                continue;
            }
            let Some(fact) = capture.baseline.facts.get(key) else {
                continue;
            };
            let Some(scope) = scope(key) else { continue };
            let stem = key.rsplit_once('#').map_or(key.as_str(), |(stem, _)| stem);
            if capture.ambiguous.contains(stem) {
                continue;
            }
            let shape = capture.shapes.get(key).unwrap_or(&fact.body);
            let group = hash_serialized(&(scope, &fact.signature, shape))?;
            let entry = groups.entry(group).or_default();
            if side == 0 {
                entry.0.push(key.clone());
            } else {
                entry.1.push(key.clone());
            }
        }
    }
    let mut result = pair_groups(groups, false);
    // Establish source callable correspondence before grouping any lambdas.
    let mut owner_pairs = BTreeMap::new();
    for (old, new) in &result {
        visits += 1;
        let owner = |key: &str| {
            let (module, rest) = QualifiedName::new(key).split_first()?;
            let (kind, name) = QualifiedName::new(rest).split_first()?;
            if !matches!(kind, "function" | "method") {
                return None;
            }
            let name = name.rsplit_once('#').map_or(name, |(name, _)| name);
            Some(QualifiedName::from_segments([
                module, "function", "lambda", name,
            ]))
        };
        if let (Some(old), Some(new)) = (owner(old), owner(new)) {
            owners.extend([old.clone(), new.clone()]);
            owner_pairs.insert(old, new);
        }
    }
    // Dispatch summaries inherit a source method's correspondence. This also
    // disambiguates simultaneous renames of equal-shaped dispatch families.
    let mut inherited = Vec::new();
    for (old, new) in &result {
        visits += 1;
        if QualifiedName::new(old).contains_run(&METHOD_RUN) {
            for prefix in [&*DISPATCH_RUN, &*FUNCTION_RUN] {
                let old = QualifiedName::new(old).replace_run(&METHOD_RUN, prefix);
                let new = QualifiedName::new(new).replace_run(&METHOD_RUN, prefix);
                let old = old.rsplit_once('#').map_or(old.as_str(), |(stem, _)| stem);
                let new = new.rsplit_once('#').map_or(new.as_str(), |(stem, _)| stem);
                if before.baseline.facts.contains_key(old)
                    && after.baseline.facts.contains_key(new)
                    && !after.baseline.facts.contains_key(old)
                    && !before.baseline.facts.contains_key(new)
                {
                    inherited.push((old.to_owned(), new.to_owned()));
                }
            }
        }
    }
    result.extend(inherited);
    let mut expanded = keys.clone();
    let mut groups = Groups::new();
    for (side, capture) in [(0, before), (1, after)] {
        for owner in &owners {
            let canonical = if side == 0 {
                owner_pairs.get(owner).unwrap_or(owner)
            } else {
                owner
            };
            for key in capture.lambda_owners.get(owner).into_iter().flatten() {
                visits += 1;
                expanded.insert(key.clone());
                let fact = &capture.baseline.facts[key];
                let shape = capture.shapes.get(key).unwrap_or(&fact.body);
                let group = hash_serialized(&(canonical, &fact.signature, shape))?;
                let entry = groups.entry(group).or_default();
                if side == 0 {
                    entry.0.push(key.clone());
                } else {
                    entry.1.push(key.clone());
                }
            }
        }
    }
    result.extend(pair_groups(groups, true));
    Ok((result, expanded, visits))
}

pub(super) fn targets(callees: &[String], pairs: &BTreeMap<String, String>) -> Vec<String> {
    let mut result: Vec<_> = callees
        .iter()
        .map(|key| pairs.get(key).unwrap_or(key).clone())
        .collect();
    result.sort();
    result.dedup();
    result
}

pub(super) fn human_name(name: &str) -> String {
    let name = name.replace("$dispatch$", "");
    if let Some((prefix, _)) = name.split_once("<function-value candidates:") {
        return format!("{prefix}<function value call>");
    }
    if let Some((prefix, lambda)) = name.split_once("<lambda ")
        && let Some((owner, _)) = lambda.rsplit_once('@')
    {
        return format!("{prefix}<lambda in {owner}>");
    }
    let name = name
        .rsplit_once('#')
        .filter(|(_, suffix)| suffix.chars().all(|c| c.is_ascii_digit()))
        .map_or(name.as_str(), |(stem, _)| stem);
    super::super::super::generics::source_spelling(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_diff_rename_candidate_work_scales() {
        let (template, _) =
            super::super::tests::captures("fn f() {} fn main() {}", "fn f() {} fn main() {}");
        for n in [16, 64, 256, 1024] {
            let mut before = template.clone();
            let mut after = template.clone();
            let fact = before.baseline.facts["main.wi::function::f#0"].clone();
            before.baseline.facts.clear();
            after.baseline.facts.clear();
            before.shapes.clear();
            after.shapes.clear();
            let mut keys = BTreeSet::new();
            for i in 0..n {
                let old = format!("m{i}.wi::function::old#0");
                let new = format!("m{i}.wi::function::new#0");
                before.baseline.facts.insert(old.clone(), fact.clone());
                after.baseline.facts.insert(new.clone(), fact.clone());
                keys.extend([old, new]);
            }
            let (pairs, _, visits) = pairs_with_work(&before, &after, &keys).unwrap();
            assert_eq!(pairs.len(), n);
            assert_eq!(visits, 6 * n);
            println!("rename pairs={n} candidate_and_inheritance_visits={visits}");
        }
    }

    #[test]
    fn semantic_diff_rename_lambda_owner_work_scales() {
        let (template, _) =
            super::super::tests::captures("fn f() {} fn main() {}", "fn f() {} fn main() {}");
        for n in [16, 64, 256, 1024] {
            let mut before = template.clone();
            let mut after = template.clone();
            let fact = before.baseline.facts["main.wi::function::f#0"].clone();
            let mut lambda = (*fact).clone();
            lambda.signature = None;
            let lambda = Arc::new(lambda);
            before.baseline.facts.clear();
            after.baseline.facts.clear();
            before.shapes.clear();
            after.shapes.clear();
            let mut keys = BTreeSet::new();
            for i in 0..n {
                for (capture, name) in [(&mut before, "old"), (&mut after, "new")] {
                    let owner = format!("m{i}.wi::function::{name}#0");
                    let child = format!("m{i}.wi::function::<lambda {name}@10:20>");
                    capture.baseline.facts.insert(owner.clone(), fact.clone());
                    capture.baseline.facts.insert(child.clone(), lambda.clone());
                    capture
                        .lambda_owners
                        .entry(scope(&child).unwrap())
                        .or_default()
                        .insert(child.clone());
                    keys.extend([owner, child]);
                    // Unrelated owners must not be scanned for each changed owner.
                    for j in 0..4 {
                        let other = format!("unrelated{i}_{j}.wi::function::<lambda keep@10:20>");
                        capture.baseline.facts.insert(other.clone(), lambda.clone());
                        capture
                            .lambda_owners
                            .entry(scope(&other).unwrap())
                            .or_default()
                            .insert(other);
                    }
                }
            }
            let (pairs, expanded, visits) = pairs_with_work(&before, &after, &keys).unwrap();
            assert_eq!(pairs.len(), 2 * n);
            assert_eq!(expanded, keys);
            assert_eq!(visits, 12 * n);
            println!(
                "rename owners={n} paired_lambdas={n} unrelated_lambdas={} visits={visits}",
                4 * n
            );
        }
    }

    #[test]
    fn semantic_diff_rename_nested_shape_storage_scales() {
        use super::super::super::super::TokenHashes;
        for n in [16, 64, 256, 1024, 4096] {
            let source = format!("{}name + 1{}", "(".repeat(n), ")".repeat(n));
            let renamed = source.replace("name", "longer_name");
            let old = TokenHashes::with_identifier_shape(&source, true).unwrap();
            let new = TokenHashes::with_identifier_shape(&renamed, true).unwrap();
            assert_eq!(old.prefix.len(), 2 * n + 4);
            assert!(old.names.is_empty());
            for depth in 0..n {
                assert_eq!(
                    old.range(crate::diagnostics::Span::new(
                        depth,
                        source.len() - depth,
                        1,
                        1
                    )),
                    new.range(crate::diagnostics::Span::new(
                        depth,
                        renamed.len() - depth,
                        1,
                        1
                    ))
                );
            }
            println!(
                "rename nested_depth={n} prefix_entries={} ranges={n}",
                old.prefix.len()
            );
        }
    }

    #[test]
    fn semantic_diff_rename_human_names_cover_evidence() {
        let report = SemanticDiff {
            schema: "willow.semantic-diff.v1",
            status: "complete",
            base: Value::Null,
            target: Value::Null,
            changes: vec![
                json!({"symbol":"main.wi::function::$dispatch$C::f", "cause":[{"chain":["::function::<function-value candidates: arity 2, type (i,@6:Value,)->i>", "main.wi::function::<lambda main@40:50>"]}], "impacted_callers":["main.wi::method::C::f#0"]}),
            ],
            summary: json!({}),
        };
        let text = report.human(true);
        for internal in ["$dispatch$", "@6", "@40", "#0", "function-value candidates"] {
            assert!(!text.contains(internal), "{text}");
        }
        assert!(text.contains("function value call"));
        assert!(text.contains("lambda in main"));
    }
}
