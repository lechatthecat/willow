//! Lossless JSON changes against one immutable full snapshot. No delta chains.
use super::*;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
enum Step {
    Field(String),
    Index(usize),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
enum Change {
    Set {
        path: Vec<Step>,
        value: Value,
    },
    Remove {
        path: Vec<Step>,
    },
    Splice {
        path: Vec<Step>,
        start: usize,
        delete: usize,
        values: Vec<Value>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Delta {
    snapshot_encoding: String,
    base: String,
    base_revision: String,
    revision: String,
    changes: Vec<Change>,
}

impl Change {
    fn path(&mut self) -> &mut Vec<Step> {
        match self {
            Self::Set { path, .. } | Self::Remove { path } | Self::Splice { path, .. } => path,
        }
    }
}

const ENCODING: &str = "snapshot-delta-v1";

pub(super) fn encode(
    before: &Snapshot,
    after: &Snapshot,
    base: &Path,
    path: &Path,
) -> Result<Value> {
    let mut changes = Vec::new();
    // Compare compiler-owned records directly; serialize only changed records.
    diff_value(
        &before.revision,
        &after.revision,
        &mut field_path(&["revision"]),
        &mut changes,
    )?;
    diff_map(
        &before.sources,
        &after.sources,
        &mut field_path(&["sources"]),
        &mut changes,
    )?;
    diff_array(
        &before.functions,
        &after.functions,
        function_equal,
        &mut field_path(&["functions"]),
        &mut changes,
    )?;
    macro_rules! array {
        ($field:ident) => {
            diff_array(
                &before.semantic.$field,
                &after.semantic.$field,
                PartialEq::eq,
                &mut field_path(&["semantic", stringify!($field)]),
                &mut changes,
            )?;
        };
    }
    array!(modules);
    array!(symbols);
    array!(references);
    array!(expressions);
    array!(flows);
    diff_map(
        &before.semantic.witnesses,
        &after.semantic.witnesses,
        &mut field_path(&["semantic", "witnesses"]),
        &mut changes,
    )?;
    diff_map(
        &before.semantic.compiler_effects,
        &after.semantic.compiler_effects,
        &mut field_path(&["semantic", "compiler_effects"]),
        &mut changes,
    )?;
    serde_json::to_value(Delta {
        snapshot_encoding: ENCODING.into(),
        base: base_reference(base, path)?,
        base_revision: before.revision.clone(),
        revision: after.revision.clone(),
        changes,
    })
    .map_err(Into::into)
}

fn field_path(fields: &[&str]) -> Vec<Step> {
    fields
        .iter()
        .map(|field| Step::Field((*field).into()))
        .collect()
}

fn function_equal(a: &Function, b: &Function) -> bool {
    // Function also carries four #[serde(skip)] frontend-only fields. They must
    // not turn every loaded function into a change on each save.
    a.identity == b.identity
        && a.id == b.id
        && a.module == b.module
        && a.name == b.name
        && a.locations == b.locations
        && a.synthetic == b.synthetic
        && a.fingerprint == b.fingerprint
        && a.body_fingerprint == b.body_fingerprint
        && a.callees == b.callees
        && a.runtime_effects == b.runtime_effects
        && a.unknown == b.unknown
        && a.unresolved == b.unresolved
}

fn diff_value<T: Serialize>(
    a: &T,
    b: &T,
    path: &mut Vec<Step>,
    changes: &mut Vec<Change>,
) -> Result<()> {
    difference(
        &serde_json::to_value(a)?,
        &serde_json::to_value(b)?,
        path,
        changes,
    );
    Ok(())
}

fn diff_array<T: Serialize>(
    a: &[T],
    b: &[T],
    equal: impl Fn(&T, &T) -> bool,
    path: &mut Vec<Step>,
    changes: &mut Vec<Change>,
) -> Result<()> {
    if a.len() != b.len() {
        // Insert/delete uses one splice. Equality comparisons visit disjoint
        // records; the unchanged prefix/suffix is never serialized.
        let start = a.iter().zip(b).take_while(|(a, b)| equal(a, b)).count();
        let suffix = a[start..]
            .iter()
            .rev()
            .zip(b[start..].iter().rev())
            .take_while(|(a, b)| equal(a, b))
            .count();
        changes.push(Change::Splice {
            path: path.clone(),
            start,
            delete: a.len() - start - suffix,
            values: b[start..b.len() - suffix]
                .iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<_, _>>()?,
        });
    } else {
        for (index, (a, b)) in a.iter().zip(b).enumerate() {
            if !equal(a, b) {
                path.push(Step::Index(index));
                diff_value(a, b, path, changes)?;
                path.pop();
            }
        }
    }
    Ok(())
}

fn diff_map<T: Serialize + PartialEq>(
    a: &BTreeMap<String, T>,
    b: &BTreeMap<String, T>,
    path: &mut Vec<Step>,
    changes: &mut Vec<Change>,
) -> Result<()> {
    for (key, old, new) in merge_maps(a.iter(), b.iter()) {
        if old == new {
            continue;
        }
        path.push(Step::Field(key.clone()));
        match (old, new) {
            (Some(old), Some(new)) => diff_value(old, new, path, changes)?,
            (None, Some(new)) => changes.push(Change::Set {
                path: path.clone(),
                value: serde_json::to_value(new)?,
            }),
            (Some(_), None) => changes.push(Change::Remove { path: path.clone() }),
            (None, None) => unreachable!(),
        }
        path.pop();
    }
    Ok(())
}

// Both maps already have sorted keys. Merge them once instead of doing a
// logarithmic lookup for every source, witness or compiler-effect entry.
fn merge_maps<'a, T: 'a>(
    a: impl Iterator<Item = (&'a String, &'a T)>,
    b: impl Iterator<Item = (&'a String, &'a T)>,
) -> impl Iterator<Item = (&'a String, Option<&'a T>, Option<&'a T>)> {
    let mut a = a.peekable();
    let mut b = b.peekable();
    std::iter::from_fn(move || match (a.peek(), b.peek()) {
        (Some((ka, _)), Some((kb, _))) if ka == kb => {
            let (key, old) = a.next().unwrap();
            Some((key, Some(old), Some(b.next().unwrap().1)))
        }
        (Some((ka, _)), Some((kb, _))) if ka < kb => {
            a.next().map(|(key, old)| (key, Some(old), None))
        }
        (Some(_), None) => a.next().map(|(key, old)| (key, Some(old), None)),
        (_, Some(_)) => b.next().map(|(key, new)| (key, None, Some(new))),
        (None, None) => None,
    })
}

fn base_reference(base: &Path, output: &Path) -> Result<String> {
    let base = std::fs::canonicalize(base)?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = std::fs::canonicalize(parent)?;
    let a: Vec<_> = parent.components().collect();
    let b: Vec<_> = base.components().collect();
    let shared = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let reference = if shared == 0 {
        // Windows drive/UNC roots can differ; such a base is not relocatable.
        base
    } else {
        let mut relative = std::path::PathBuf::new();
        for _ in shared..a.len() {
            relative.push("..");
        }
        for component in &b[shared..] {
            relative.push(component.as_os_str());
        }
        relative
    };
    Ok(reference
        .to_str()
        .context("non UTF-8 delta base path")?
        .into())
}

// Do not compare entire equal-sized subtrees before descending: that repeats
// work at every enclosing level on deep inputs. Each scalar is visited once.
fn difference(
    before: &Value,
    after: &Value,
    path: &mut Vec<Step>,
    changes: &mut Vec<Change>,
) -> usize {
    let mut visits = 1;
    match (before, after) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, old, new) in merge_maps(a.iter(), b.iter()) {
                path.push(Step::Field(key.clone()));
                match (old, new) {
                    (Some(old), Some(new)) => visits += difference(old, new, path, changes),
                    (None, Some(new)) => changes.push(Change::Set {
                        path: path.clone(),
                        value: new.clone(),
                    }),
                    (Some(_), None) => changes.push(Change::Remove { path: path.clone() }),
                    (None, None) => unreachable!(),
                }
                path.pop();
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (index, (old, new)) in a.iter().zip(b).enumerate() {
                path.push(Step::Index(index));
                visits += difference(old, new, path, changes);
                path.pop();
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            // A single splice, with disjoint prefix/suffix scans. No LCS table
            // or repeated Vec insertion/removal, even for fragmented changes.
            let start = a.iter().zip(b).take_while(|(a, b)| a == b).count();
            let suffix = a[start..]
                .iter()
                .rev()
                .zip(b[start..].iter().rev())
                .take_while(|(a, b)| a == b)
                .count();
            changes.push(Change::Splice {
                path: path.clone(),
                start,
                delete: a.len() - start - suffix,
                values: b[start..b.len() - suffix].to_vec(),
            });
        }
        _ if before == after => {}
        _ => changes.push(Change::Set {
            path: path.clone(),
            value: after.clone(),
        }),
    }
    visits
}

fn at<'a>(mut value: &'a mut Value, path: &[Step]) -> Result<&'a mut Value> {
    for step in path {
        value = match step {
            Step::Field(key) => value.as_object_mut().and_then(|map| map.get_mut(key)),
            Step::Index(index) => value.as_array_mut().and_then(|items| items.get_mut(*index)),
        }
        .context("invalid delta path")?;
    }
    Ok(value)
}

fn validate_changes(changes: &[Change]) -> Result<()> {
    // Generated changes never overlap. Reject hostile inputs that repeatedly
    // splice the same large array or replace parents after editing children.
    #[derive(Default)]
    struct Paths {
        terminal: bool,
        children: HashMap<Step, Paths>,
    }
    let mut paths = Paths::default();
    for change in changes {
        let path = match change {
            Change::Set { path, .. } | Change::Remove { path } | Change::Splice { path, .. } => {
                path
            }
        };
        ensure!(path.len() <= 128, "delta path exceeds depth limit");
        let mut node = &mut paths;
        for step in path {
            ensure!(!node.terminal, "overlapping delta paths");
            node = node.children.entry(step.clone()).or_default();
        }
        ensure!(
            !node.terminal && node.children.is_empty(),
            "overlapping delta paths"
        );
        node.terminal = true;
    }
    Ok(())
}

fn apply(value: &mut Value, changes: Vec<Change>) -> Result<()> {
    validate_changes(&changes)?;
    for change in changes {
        match change {
            Change::Set { path, value: new } => {
                if let Some((Step::Field(key), parent)) = path.split_last() {
                    at(value, parent)?
                        .as_object_mut()
                        .context("invalid delta object")?
                        .insert(key.clone(), new);
                } else {
                    *at(value, &path)? = new;
                }
            }
            Change::Remove { path } => {
                let Some((Step::Field(key), parent)) = path.split_last() else {
                    anyhow::bail!("invalid delta removal");
                };
                ensure!(
                    at(value, parent)?
                        .as_object_mut()
                        .context("invalid delta object")?
                        .remove(key)
                        .is_some(),
                    "missing delta field"
                );
            }
            Change::Splice {
                path,
                start,
                delete,
                values,
            } => {
                let array = at(value, &path)?
                    .as_array_mut()
                    .context("invalid delta array")?;
                ensure!(
                    start <= array.len() && delete <= array.len() - start,
                    "invalid delta splice"
                );
                array.splice(start..start + delete, values);
            }
        }
    }
    Ok(())
}

fn groups(changes: Vec<Change>) -> Result<BTreeMap<Step, Vec<Change>>> {
    let mut groups: BTreeMap<Step, Vec<Change>> = BTreeMap::new();
    for mut change in changes {
        ensure!(!change.path().is_empty(), "invalid delta record path");
        groups
            .entry(change.path().remove(0))
            .or_default()
            .push(change);
    }
    Ok(groups)
}

fn patch<T: Serialize + serde::de::DeserializeOwned>(
    target: &mut T,
    changes: Vec<Change>,
) -> Result<()> {
    let mut value = serde_json::to_value(&*target)?;
    apply(&mut value, changes)?;
    *target = serde_json::from_value(value)?;
    Ok(())
}

fn patch_array<T: Serialize + serde::de::DeserializeOwned>(
    target: &mut Vec<T>,
    mut changes: Vec<Change>,
) -> Result<()> {
    if changes.len() == 1 && changes[0].path().is_empty() {
        let Change::Splice {
            start,
            delete,
            values,
            ..
        } = changes.pop().unwrap()
        else {
            anyhow::bail!("invalid delta array operation");
        };
        ensure!(
            start <= target.len() && delete <= target.len() - start,
            "invalid delta splice"
        );
        let values = values
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<T>, _>>()?;
        target.splice(start..start + delete, values);
    } else {
        for (step, changes) in groups(changes)? {
            let Step::Index(index) = step else {
                anyhow::bail!("invalid delta array index");
            };
            patch(
                target.get_mut(index).context("invalid delta array index")?,
                changes,
            )?;
        }
    }
    Ok(())
}

fn patch_map<T: Serialize + serde::de::DeserializeOwned>(
    target: &mut BTreeMap<String, T>,
    changes: Vec<Change>,
) -> Result<()> {
    for (step, mut changes) in groups(changes)? {
        let Step::Field(key) = step else {
            anyhow::bail!("invalid delta map key");
        };
        if changes.len() == 1 && changes[0].path().is_empty() {
            match changes.pop().unwrap() {
                Change::Set { value, .. } => {
                    target.insert(key, serde_json::from_value(value)?);
                }
                Change::Remove { .. } => {
                    ensure!(target.remove(&key).is_some(), "missing delta field");
                }
                change @ Change::Splice { .. } => patch(
                    target.get_mut(&key).context("missing delta field")?,
                    vec![change],
                )?,
            }
        } else {
            patch(
                target.get_mut(&key).context("missing delta field")?,
                changes,
            )?;
        }
    }
    Ok(())
}

fn patch_snapshot(target: &mut Snapshot, changes: Vec<Change>) -> Result<()> {
    validate_changes(&changes)?;
    for (step, changes) in groups(changes)? {
        let Step::Field(key) = step else {
            anyhow::bail!("invalid delta snapshot field");
        };
        match key.as_str() {
            "revision" => patch(&mut target.revision, changes)?,
            "sources" => patch_map(&mut target.sources, changes)?,
            "functions" => patch_array(&mut target.functions, changes)?,
            "semantic" => {
                for (step, changes) in groups(changes)? {
                    let Step::Field(key) = step else {
                        anyhow::bail!("invalid delta semantic field");
                    };
                    match key.as_str() {
                        "modules" => patch_array(&mut target.semantic.modules, changes)?,
                        "symbols" => patch_array(&mut target.semantic.symbols, changes)?,
                        "references" => patch_array(&mut target.semantic.references, changes)?,
                        "expressions" => patch_array(&mut target.semantic.expressions, changes)?,
                        "flows" => patch_array(&mut target.semantic.flows, changes)?,
                        "witnesses" => patch_map(&mut target.semantic.witnesses, changes)?,
                        "compiler_effects" => {
                            patch_map(&mut target.semantic.compiler_effects, changes)?
                        }
                        _ => anyhow::bail!("invalid delta semantic field"),
                    }
                }
            }
            _ => anyhow::bail!("invalid delta snapshot field"),
        }
    }
    Ok(())
}

pub(super) fn decode(
    value: Value,
    path: &Path,
    cached: Option<(&Path, &Snapshot)>,
) -> Result<Snapshot> {
    let delta: Delta = serde_json::from_value(value)?;
    ensure!(
        delta.snapshot_encoding == ENCODING,
        "unsupported snapshot encoding"
    );
    let base_path = path.parent().unwrap_or(Path::new(".")).join(&delta.base);
    let mut base = if let Some((known_path, known)) = cached
        && std::fs::canonicalize(&base_path)? == std::fs::canonicalize(known_path)?
    {
        known.clone()
    } else {
        load_full(&base_path).context("load delta base (keep the full base file)")?
    };
    ensure!(
        base.revision == delta.base_revision,
        "delta base revision mismatch"
    );
    patch_snapshot(&mut base, delta.changes)?;
    ensure!(base.revision == delta.revision, "delta revision mismatch");
    // Count expanded bytes during digest validation, not with another full scan.
    base.validate_with_limit(Some(MAX_BYTES))?;
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typed_records_skip_unchanged_serialization_and_patch_maps_and_splices() {
        struct Record<'a>(u64, &'a std::cell::Cell<usize>);
        impl Serialize for Record<'_> {
            fn serialize<S: serde::Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                self.1.set(self.1.get() + 1);
                serializer.serialize_u64(self.0)
            }
        }
        for n in [16, 64, 256, 1024] {
            let serialized = std::cell::Cell::new(0);
            let compared = std::cell::Cell::new(0);
            let before: Vec<_> = (0..n).map(|i| Record(i as u64, &serialized)).collect();
            let mut after: Vec<_> = (0..n).map(|i| Record(i as u64, &serialized)).collect();
            after[n - 1].0 += 1;
            let mut changes = Vec::new();
            diff_array(
                &before,
                &after,
                |a, b| {
                    compared.set(compared.get() + 1);
                    a.0 == b.0
                },
                &mut vec![],
                &mut changes,
            )
            .unwrap();
            assert_eq!(compared.get(), n);
            assert_eq!(serialized.get(), 2);
            let mut loaded: Vec<u64> = before.iter().map(|r| r.0).collect();
            patch_array(&mut loaded, changes).unwrap();
            assert_eq!(loaded, after.iter().map(|r| r.0).collect::<Vec<_>>());
            println!(
                "delta records n={n} comparisons={} serialized={}",
                compared.get(),
                serialized.get()
            );
        }
        for (mut before, after) in [
            (vec![1, 2], vec![1, 3, 2]),
            (vec![1, 2, 3], vec![1, 3]),
            (vec![], vec![1]),
            (vec![1], vec![]),
        ] {
            let mut changes = Vec::new();
            diff_array(&before, &after, PartialEq::eq, &mut vec![], &mut changes).unwrap();
            patch_array(&mut before, changes).unwrap();
            assert_eq!(before, after);
        }
        let mut before = BTreeMap::from([("a".into(), json!([1, 2])), ("b".into(), json!(1))]);
        let after = BTreeMap::from([("a".into(), json!([1, 3, 2])), ("c".into(), json!({"x":1}))]);
        let mut changes = Vec::new();
        diff_map(&before, &after, &mut vec![], &mut changes).unwrap();
        patch_map(&mut before, changes).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn structural_delta_roundtrips_and_rejects_overlapping_work() {
        let pairs = [
            (json!(null), json!(true)),
            (json!(true), json!(false)),
            (json!(1), json!(2)),
            (json!("a"), json!("日本語")),
            (json!(1), json!([1])),
            (json!({}), json!({"x":1})),
            (json!({"x":1}), json!({})),
            (json!({"x":null}), json!({"x":false})),
            (json!({"a":1}), json!({"b":2})),
            (json!([]), json!([1, 2])),
            (json!([1, 2]), json!([])),
            (json!([1, 2]), json!([0, 1, 2])),
            (json!([1, 2]), json!([1, 2, 3])),
            (json!([1, 2, 3]), json!([1, 3])),
            (json!([1, 3]), json!([1, 2, 3])),
            (json!([1, 2, 3, 4]), json!([0, 2, 3, 5])),
            (json!([{"x":1}]), json!([{"x":2}])),
            (json!({"0":{"/":1}}), json!({"0":{"/":2}})),
            (json!([1, 2, 3]), json!([3, 2, 1])),
            (json!([1, 2, 3]), json!([1, 2, 3])),
        ];
        for (mut before, after) in pairs {
            let mut changes = Vec::new();
            difference(&before, &after, &mut Vec::new(), &mut changes);
            let wire = serde_json::to_vec(&changes).unwrap();
            apply(&mut before, serde_json::from_slice(&wire).unwrap()).unwrap();
            assert_eq!(before, after);
        }
        for paths in [
            vec![vec![], vec![]],
            vec![vec![], vec![Step::Index(0)]],
            vec![vec![Step::Index(0)], vec![]],
        ] {
            let changes = paths
                .into_iter()
                .map(|path| Change::Set {
                    path,
                    value: json!(0),
                })
                .collect();
            assert!(apply(&mut json!([0]), changes).is_err());
        }
        for change in [
            Change::Remove { path: vec![] },
            Change::Remove {
                path: vec![Step::Field("absent".into())],
            },
            Change::Set {
                path: vec![Step::Index(9)],
                value: json!(0),
            },
            Change::Splice {
                path: vec![],
                start: 0,
                delete: usize::MAX,
                values: vec![],
            },
            Change::Splice {
                path: vec![],
                start: usize::MAX,
                delete: 0,
                values: vec![],
            },
        ] {
            assert!(apply(&mut json!([0]), vec![change]).is_err());
        }
    }

    #[test]
    fn diff_visit_counts_are_linear_for_wide_and_deep_changes() {
        for n in [16, 32, 64, 128] {
            let before = json!(vec![json!({"a":1,"b":2}); n]);
            let mut after = before.clone();
            after[n - 1]["a"] = json!(3);
            let mut changes = Vec::new();
            let visits = difference(&before, &after, &mut Vec::new(), &mut changes);
            assert_eq!(visits, 1 + 3 * n);
            assert_eq!(changes.len(), 1);
            println!("delta wide n={n} visits={visits} changes={}", changes.len());
        }
        for n in [8, 16, 32, 64] {
            let mut before = json!(1);
            let mut after = json!(2);
            for _ in 0..n {
                before = json!([before]);
                after = json!([after]);
            }
            let mut changes = Vec::new();
            let visits = difference(&before, &after, &mut Vec::new(), &mut changes);
            assert_eq!(visits, n + 1);
            assert_eq!(changes.len(), 1);
            apply(&mut before, changes).unwrap();
            assert_eq!(before, after);
            println!("delta deep n={n} visits={visits}");
        }
    }
}
