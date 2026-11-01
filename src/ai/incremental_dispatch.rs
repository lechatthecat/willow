//! Retained virtual unions. The declaration forest is immutable between nominal
//! changes; body edits replace query memberships and repair only changed edges.
use super::*;
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

type Query = (String, String);

#[derive(Default)]
struct Forest {
    classes: Vec<dispatch::Class>,
    by_key: HashMap<String, usize>,
    parent: Vec<Option<usize>>,
    start: Vec<usize>,
    end: Vec<usize>,
    order: Vec<usize>,
    head: Vec<usize>,
    position: Vec<usize>,
    declarations: HashMap<String, im::OrdMap<usize, (usize, String)>>,
}
impl Forest {
    fn new(classes: &[dispatch::Class]) -> Self {
        let mut this = Self {
            classes: classes.to_vec(),
            by_key: classes
                .iter()
                .enumerate()
                .map(|(i, c)| (c.key.clone(), i))
                .collect(),
            parent: vec![None; classes.len()],
            start: vec![0; classes.len()],
            end: vec![0; classes.len()],
            head: vec![0; classes.len()],
            position: vec![0; classes.len()],
            ..Self::default()
        };
        let mut children = vec![Vec::new(); classes.len()];
        let mut roots = Vec::new();
        for (i, c) in classes.iter().enumerate() {
            this.parent[i] = c.base.as_ref().and_then(|b| this.by_key.get(b)).copied();
            if let Some(parent) = this.parent[i] {
                children[parent].push(i);
            } else {
                roots.push(i);
            }
        }
        let mut sizes = vec![1; classes.len()];
        let mut heavy = vec![None; classes.len()];
        let mut work: Vec<_> = roots.iter().copied().map(|i| (i, false)).collect();
        while let Some((i, exit)) = work.pop() {
            if exit {
                this.end[i] = this.order.len();
                for &child in &children[i] {
                    sizes[i] += sizes[child];
                    if heavy[i].is_none_or(|old| sizes[child] > sizes[old]) {
                        heavy[i] = Some(child);
                    }
                }
            } else {
                this.start[i] = this.order.len();
                this.order.push(i);
                work.push((i, true));
                work.extend(children[i].iter().map(|&child| (child, false)));
            }
        }
        let mut work: Vec<_> = roots.into_iter().map(|i| (i, i)).collect();
        let mut position = 0;
        while let Some((i, head)) = work.pop() {
            this.head[i] = head;
            this.position[i] = position;
            position += 1;
            work.extend(
                children[i]
                    .iter()
                    .filter(|&&c| Some(c) != heavy[i])
                    .map(|&c| (c, c)),
            );
            if let Some(child) = heavy[i] {
                work.push((child, head));
            }
        }
        for (i, c) in classes.iter().enumerate() {
            for (method, id) in &c.methods {
                this.declarations
                    .entry(method.clone())
                    .or_default()
                    .insert(this.position[i], (i, id.clone()));
            }
        }
        this
    }
    // A root path crosses at most logarithmically many heavy-path segments.
    fn nearest<T>(
        &self,
        points: &im::OrdMap<usize, T>,
        mut class: usize,
        visits: &mut usize,
    ) -> Option<usize> {
        loop {
            *visits += 1;
            let head = self.head[class];
            if let Some((&position, _)) = points
                .range(self.position[head]..=self.position[class])
                .next_back()
            {
                return Some(position);
            }
            class = self.parent[head]?;
        }
    }
    fn id(&self, class: usize, method: &str) -> String {
        let class = &self.classes[class];
        hash(
            serde_json::to_vec(&(&class.module, format!("$dispatch${}::{method}", class.name)))
                .unwrap(),
        )
    }
    fn function(&self, class: usize, method: &str, point: &Point, visits: &mut usize) -> Function {
        let inherited = self.declarations.get(method).and_then(|declarations| {
            self.nearest(declarations, class, visits)
                .map(|position| declarations[&position].1.clone())
        });
        *visits += point.children.len();
        let mut callees: Vec<_> = inherited
            .iter()
            .cloned()
            .chain(point.children.iter().map(|&child| self.id(child, method)))
            .collect();
        callees.sort();
        callees.dedup();
        let missing = inherited.is_none();
        Function {
            identity: None,
            body_id: None,
            body_location: None,
            rename_calls: vec![],
            rename_imports: vec![],
            id: self.id(class, method),
            module: self.classes[class].module.clone(),
            name: format!("$dispatch${}::{method}", self.classes[class].name),
            locations: vec![],
            synthetic: true,
            fingerprint: String::new(),
            body_fingerprint: String::new(),
            callees,
            runtime_effects: if missing {
                RuntimeEffects::ALL.bits()
            } else {
                0
            },
            unknown: missing,
            unresolved: if missing {
                vec!["unresolved-dispatch".into()]
            } else {
                vec![]
            },
        }
    }
}

#[derive(Clone, Default)]
struct Point {
    children: im::OrdSet<usize>,
}
#[derive(Clone, Default)]
struct Method {
    // Euler positions permit skipping whole descendant subtrees; heavy-path
    // positions find the nearest relevant ancestor without walking deep chains.
    points: im::OrdMap<usize, Point>,
    marked: im::OrdMap<usize, usize>,
}
impl Method {
    fn parent(&self, forest: &Forest, class: usize, visits: &mut usize) -> Option<usize> {
        let parent = forest.parent[class]?;
        forest
            .nearest(&self.marked, parent, visits)
            .map(|position| self.marked[&position])
    }
    fn insert(
        &mut self,
        forest: &Forest,
        class: usize,
        touched: &mut BTreeSet<usize>,
        visits: &mut usize,
    ) {
        let at = forest.start[class];
        if self.points.contains_key(&at) {
            return;
        }
        *visits += 1;
        let parent = self.parent(forest, class, visits);
        let mut point = Point::default();
        let mut cursor = at;
        while let Some((&position, _)) = self.points.range(cursor..forest.end[class]).next() {
            *visits += 1;
            let child = forest.order[position];
            point.children.insert(child);
            cursor = forest.end[child];
        }
        if let Some(parent) = parent {
            let parent_point = self.points.get_mut(&forest.start[parent]).unwrap();
            for &child in &point.children {
                parent_point.children.remove(&child);
            }
            parent_point.children.insert(class);
            touched.insert(parent);
        }
        self.points.insert(at, point);
        self.marked.insert(forest.position[class], class);
        touched.insert(class);
    }
    fn remove(
        &mut self,
        forest: &Forest,
        class: usize,
        touched: &mut BTreeSet<usize>,
        visits: &mut usize,
    ) {
        let Some(point) = self.points.remove(&forest.start[class]) else {
            return;
        };
        *visits += 1;
        self.marked.remove(&forest.position[class]);
        if let Some(parent) = self.parent(forest, class, visits) {
            let parent_point = self.points.get_mut(&forest.start[parent]).unwrap();
            parent_point.children.remove(&class);
            *visits += point.children.len();
            parent_point.children.extend(point.children);
            touched.insert(parent);
        }
        touched.insert(class);
    }
}

#[derive(Clone, Default)]
pub(super) struct State {
    forest: Arc<Forest>,
    owners: im::OrdMap<String, im::OrdSet<Query>>,
    counts: im::OrdMap<Query, usize>,
    queries: im::OrdMap<String, im::OrdSet<String>>,
    methods: im::OrdMap<String, Method>,
    functions: im::OrdMap<String, Arc<Function>>,
}
pub(super) struct Delta {
    pub functions: Vec<Function>,
    pub removed: Vec<String>,
    pub visits: usize,
}
impl State {
    pub(super) fn new(
        classes: &[dispatch::Class],
        owners: HashMap<String, HashSet<Query>>,
    ) -> Self {
        let mut this = Self {
            forest: Arc::new(Forest::new(classes)),
            ..Self::default()
        };
        if this.supported() {
            this.update(owners.into_iter().collect());
        }
        this
    }
    pub(super) fn supported(&self) -> bool {
        self.forest.order.len() == self.forest.classes.len()
    }
    pub(super) fn query(&self, class: &str, method: &str) -> Option<&str> {
        if !self
            .counts
            .contains_key(&(class.to_owned(), method.to_owned()))
        {
            return None;
        }
        let class = *self.forest.by_key.get(class)?;
        self.functions
            .get(&self.forest.id(class, method))
            .map(|f| f.id.as_str())
    }
    pub(super) fn function(&self, id: &str) -> Option<&Function> {
        self.functions.get(id).map(Arc::as_ref)
    }
    pub(super) fn functions(&self) -> impl Iterator<Item = &Function> {
        self.functions.values().map(Arc::as_ref)
    }
    pub(super) fn len(&self) -> usize {
        self.functions.len()
    }
    pub(super) fn update(&mut self, updates: Vec<(String, HashSet<Query>)>) -> Delta {
        let mut changes = BTreeMap::<Query, isize>::new();
        let mut visits = 0;
        for (owner, queries) in updates {
            let queries: im::OrdSet<_> = queries.into_iter().collect();
            let old = self.owners.remove(&owner).unwrap_or_default();
            visits += old.len() + queries.len();
            for query in &old {
                if !queries.contains(query) {
                    *changes.entry(query.clone()).or_default() -= 1;
                }
            }
            for query in &queries {
                if !old.contains(query) {
                    *changes.entry(query.clone()).or_default() += 1;
                }
            }
            if !queries.is_empty() {
                self.owners.insert(owner, queries);
            }
        }
        let mut methods = BTreeMap::<String, Vec<(String, bool)>>::new();
        for (query, delta) in changes {
            visits += 1;
            if delta == 0 {
                continue;
            }
            let old = self.counts.get(&query).copied().unwrap_or(0);
            let new = old
                .checked_add_signed(delta)
                .expect("dispatch query membership");
            if new == 0 {
                self.counts.remove(&query);
            } else {
                self.counts.insert(query.clone(), new);
            }
            if (old == 0) != (new == 0) {
                let (class, method) = query;
                if new == 0 {
                    if let Some(queries) = self.queries.get_mut(&method) {
                        queries.remove(&class);
                    }
                } else {
                    self.queries
                        .entry(method.clone())
                        .or_default()
                        .insert(class.clone());
                }
                methods.entry(method).or_default().push((class, new != 0));
            }
        }
        let mut result = Delta {
            functions: vec![],
            removed: vec![],
            visits,
        };
        for (method, changes) in methods {
            let previous = self.methods.remove(&method);
            let was_active = previous.is_some();
            let mut points = previous.unwrap_or_default();
            let mut touched = BTreeSet::new();
            let active = self.queries.get(&method).is_some_and(|q| !q.is_empty());
            if !active {
                self.queries.remove(&method);
                for &position in points.points.keys() {
                    result.visits += 1;
                    let id = self.forest.id(self.forest.order[position], &method);
                    self.functions.remove(&id);
                    result.removed.push(id);
                }
                continue;
            }
            if !was_active {
                if let Some(declarations) = self.forest.declarations.get(&method) {
                    for &(class, _) in declarations.values() {
                        points.insert(&self.forest, class, &mut touched, &mut result.visits);
                    }
                }
                for class in &self.queries[&method] {
                    if let Some(&class) = self.forest.by_key.get(class) {
                        points.insert(&self.forest, class, &mut touched, &mut result.visits);
                    }
                }
            } else {
                for (class, added) in changes {
                    let Some(&class) = self.forest.by_key.get(&class) else {
                        continue;
                    };
                    if added {
                        points.insert(&self.forest, class, &mut touched, &mut result.visits);
                    } else if !self
                        .forest
                        .declarations
                        .get(&method)
                        .is_some_and(|d| d.contains_key(&self.forest.position[class]))
                    {
                        points.remove(&self.forest, class, &mut touched, &mut result.visits);
                    }
                }
            }
            for class in touched {
                if let Some(point) = points.points.get(&self.forest.start[class]) {
                    let function = self
                        .forest
                        .function(class, &method, point, &mut result.visits);
                    self.functions
                        .insert(function.id.clone(), Arc::new(function.clone()));
                    result.functions.push(function);
                } else {
                    let id = self.forest.id(class, &method);
                    self.functions.remove(&id);
                    result.removed.push(id);
                }
            }
            self.methods.insert(method, points);
        }
        result.functions.sort_by(|a, b| a.id.cmp(&b.id));
        result.removed.sort();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn classes(size: usize, fanout: bool) -> Vec<dispatch::Class> {
        (0..size)
            .map(|i| dispatch::Class {
                key: format!("C{i}"),
                base: (i > 0).then(|| format!("C{}", if fanout { 0 } else { i - 1 })),
                module: "module.wi".into(),
                name: format!("C{i}"),
                methods: if i == 0 || i % 7 == 0 {
                    vec![("run".into(), format!("body{i}"))]
                } else {
                    vec![]
                },
            })
            .collect()
    }
    fn query(class: usize) -> HashSet<Query> {
        HashSet::from([(format!("C{class}"), "run".into())])
    }
    fn check(state: &State, classes: &[dispatch::Class], owners: &HashMap<String, HashSet<Query>>) {
        let queries = owners.values().flat_map(|q| q.iter().cloned()).collect();
        let cold = dispatch::complete(classes, &queries);
        let actual: BTreeMap<_, _> = state
            .functions()
            .map(|f| (&f.id, serde_json::to_value(f).unwrap()))
            .collect();
        let expected: BTreeMap<_, _> = cold
            .functions
            .iter()
            .map(|f| (&f.id, serde_json::to_value(f).unwrap()))
            .collect();
        assert_eq!(actual, expected);
        for ((class, method), id) in &cold.queries {
            assert_eq!(state.query(class, method), Some(id.as_str()));
        }
    }
    #[test]
    fn retained_dispatch_query_edits_match_cold() {
        for fanout in [false, true] {
            let classes = classes(48, fanout);
            let mut owners =
                HashMap::from([("first".into(), query(5)), ("second".into(), query(30))]);
            let mut state = State::new(&classes, owners.clone());
            check(&state, &classes, &owners);
            for (owner, next) in [
                ("first", query(12)),
                ("third", query(12)),
                ("second", query(47)),
                ("first", HashSet::new()),
                ("third", HashSet::new()),
                ("second", HashSet::new()),
                ("again", query(35)),
                ("other", HashSet::from([("C11".into(), "missing".into())])),
                ("again", HashSet::from([("unknown".into(), "run".into())])),
            ] {
                let previous = state.clone();
                let old: Vec<_> = previous
                    .functions()
                    .map(|f| serde_json::to_value(f).unwrap())
                    .collect();
                owners.insert(owner.into(), next.clone());
                state.update(vec![(owner.into(), next)]);
                check(&state, &classes, &owners);
                assert_eq!(
                    old,
                    previous
                        .functions()
                        .map(|f| serde_json::to_value(f).unwrap())
                        .collect::<Vec<_>>()
                );
            }
        }
    }
    #[test]
    fn retained_dispatch_local_queries_skip_unrelated_methods_and_classes() {
        for size in [8, 32, 128] {
            let mut classes = classes(size, false);
            for (i, class) in classes.iter_mut().enumerate() {
                class.methods = vec![(format!("other{i}"), format!("otherbody{i}"))];
            }
            classes[0].methods.push(("run".into(), "runbody".into()));
            let mut owners = HashMap::from([("first".into(), query(1))]);
            let mut state = State::new(&classes, owners.clone());
            let unchanged = state.update(vec![("first".into(), query(1))]);
            assert_eq!(unchanged.visits, 2);
            assert!(unchanged.functions.is_empty());
            owners.insert("next".into(), query(2));
            let update = state.update(vec![("next".into(), query(2))]);
            assert!(update.visits <= 12, "size={size} visits={}", update.visits);
            assert_eq!(update.functions.len(), 2);
            check(&state, &classes, &owners);
            println!(
                "dispatch_retained unrelated={size} visits={} rebuilt={}",
                update.visits,
                update.functions.len()
            );
        }
    }
}
