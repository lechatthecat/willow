//! Sparse virtual unions over the whole checked class forest. Each method's
//! relevant classes (declarations + queried receivers) form a compressed tree.
//! A query follows shared union edges; it never enumerates all descendants.
use super::*;

pub(super) struct Class {
    pub key: String,
    pub base: Option<String>,
    pub module: String,
    pub name: String,
    pub methods: Vec<(String, String)>,
}

pub(super) struct Dispatch {
    pub functions: Vec<Function>,
    pub queries: HashMap<(String, String), String>,
    #[cfg(test)]
    pub visits: usize,
}

pub(super) fn complete(
    classes: &[Class],
    queries: &std::collections::HashSet<(String, String)>,
) -> Dispatch {
    let by_key: HashMap<_, _> = classes
        .iter()
        .enumerate()
        .map(|(i, c)| (c.key.as_str(), i))
        .collect();
    let mut children = vec![Vec::new(); classes.len()];
    let mut roots = Vec::new();
    for (i, class) in classes.iter().enumerate() {
        if let Some(&parent) = class.base.as_deref().and_then(|b| by_key.get(b)) {
            children[parent].push(i);
        } else {
            roots.push(i);
        }
    }
    let mut start = vec![0; classes.len()];
    let mut end = vec![0; classes.len()];
    let mut tick = 0;
    let mut work: Vec<_> = roots.into_iter().map(|i| (i, false)).collect();
    while let Some((i, exit)) = work.pop() {
        if exit {
            end[i] = tick;
        } else {
            start[i] = tick;
            tick += 1;
            work.push((i, true));
            work.extend(children[i].iter().map(|&j| (j, false)));
        }
    }
    let methods: std::collections::HashSet<_> = queries.iter().map(|(_, m)| m.as_str()).collect();
    let mut relevant = HashMap::<&str, HashMap<usize, Option<&str>>>::new();
    for (i, class) in classes.iter().enumerate() {
        for (method, id) in &class.methods {
            if methods.contains(method.as_str()) {
                relevant.entry(method).or_default().insert(i, Some(id));
            }
        }
    }
    for (class, method) in queries {
        if let Some(&i) = by_key.get(class.as_str()) {
            relevant.entry(method).or_default().entry(i).or_default();
        }
    }
    let mut result = Dispatch {
        functions: vec![],
        queries: HashMap::new(),
        #[cfg(test)]
        visits: classes.len(),
    };
    for (method, points) in relevant {
        let mut points: Vec<_> = points.into_iter().collect();
        points.sort_by_key(|(i, _)| start[*i]);
        let mut stack: Vec<(usize, usize, Option<&str>)> = Vec::new();
        for (class, own) in points {
            #[cfg(test)]
            {
                result.visits += 1;
            }
            while stack
                .last()
                .is_some_and(|(parent, _, _)| end[*parent] <= start[class])
            {
                stack.pop();
            }
            let inherited = own.or_else(|| stack.last().and_then(|(_, _, body)| *body));
            let name = format!("$dispatch${}::{method}", classes[class].name);
            let module = classes[class].module.clone();
            let id = hash(serde_json::to_vec(&(&module, &name)).unwrap());
            let position = result.functions.len();
            if let Some((_, parent, _)) = stack.last() {
                result.functions[*parent].callees.push(id.clone());
            }
            if queries.contains(&(classes[class].key.clone(), method.to_string())) {
                result
                    .queries
                    .insert((classes[class].key.clone(), method.to_string()), id.clone());
            }
            result.functions.push(Function {
                identity: None,
                body_id: None,
                body_location: None,
                rename_calls: vec![],
                rename_imports: vec![],
                id,
                module,
                name,
                locations: vec![],
                synthetic: true,
                fingerprint: String::new(),
                body_fingerprint: String::new(),
                callees: inherited.into_iter().map(str::to_string).collect(),
                runtime_effects: if inherited.is_none() {
                    RuntimeEffects::ALL.bits()
                } else {
                    0
                },
                unknown: inherited.is_none(),
                unresolved: if inherited.is_none() {
                    vec!["unresolved-dispatch".into()]
                } else {
                    vec![]
                },
            });
            stack.push((class, position, inherited));
        }
    }
    for f in &mut result.functions {
        f.callees.sort();
        f.callees.dedup();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deep_inheritance_and_fanout_share_dispatch_without_query_times_class_scans() {
        for shape in ["chain", "fanout"] {
            for n in [16, 64, 256, 1024, 4096] {
                let classes: Vec<_> = (0..n)
                    .map(|i| Class {
                        key: format!("C{i}"),
                        base: if i == 0 {
                            None
                        } else {
                            Some(format!("C{}", if shape == "chain" { i - 1 } else { 0 }))
                        },
                        module: "m".into(),
                        name: format!("C{i}"),
                        methods: if i == 0 {
                            vec![("run".into(), "body".into())]
                        } else {
                            vec![]
                        },
                    })
                    .collect();
                let queries = (0..n).map(|i| (format!("C{i}"), "run".into())).collect();
                let result = complete(&classes, &queries);
                let edges: usize = result.functions.iter().map(|f| f.callees.len()).sum();
                assert_eq!(result.visits, 2 * n);
                assert_eq!(edges, 2 * n - 1);
                assert_eq!(result.functions.len(), n);
                assert_eq!(result.queries.len(), n);
                assert!(result.functions.iter().all(|f| !f.unknown));
                println!(
                    "dispatch shape={shape} classes={n} queries={n} visits={} edges={edges}",
                    result.visits
                );
            }
        }
    }
}
