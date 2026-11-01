//! Retained verification projections. Source-coordinate repair is separate from
//! semantic rebuilding; only changed bodies and their callers are projected.
use super::*;
use crate::compiler_db::{body::BodyQueries, effects::EffectQueries, ids::BodyOwner};
use std::{
    cell::RefCell,
    collections::{HashSet, VecDeque},
    rc::Rc,
    sync::Arc,
};

type Key = (UnitId, FunctionId);

#[derive(Debug)]
pub(crate) struct VerificationSnapshotFallback;
impl std::fmt::Display for VerificationSnapshotFallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("verification snapshot scope requires full capture")
    }
}
impl std::error::Error for VerificationSnapshotFallback {}

#[derive(Debug)]
pub(crate) struct VerificationSnapshotColdFallback;
impl std::fmt::Display for VerificationSnapshotColdFallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ambiguous verification identities require cold compilation")
    }
}
impl std::error::Error for VerificationSnapshotColdFallback {}

#[derive(Debug, Clone)]
pub(crate) struct VerificationRelocation {
    pub path: String,
    pub old_start: usize,
    pub old_end: usize,
    pub new_start: usize,
    pub new_end: usize,
}

pub(crate) enum VerificationSnapshotUpdate {
    Full(Snapshot),
    Delta {
        patch: Snapshot,
        removed_functions: Vec<String>,
        removed_symbols: Vec<String>,
        relocations: Vec<VerificationRelocation>,
    },
}

#[derive(Clone, Copy)]
enum RootKind {
    Function,
    Method(usize),
    Constructor(usize),
    Default(usize),
}
#[derive(Clone)]
struct Root {
    item: usize,
    kind: RootKind,
    span: Span,
    key: Key,
}
#[derive(Clone)]
struct Unit {
    items: usize,
    imports: String,
    import_names: Arc<HashMap<String, crate::parser::ast::ImportDecl>>,
    path: String,
    unique_roots: bool,
}
#[derive(Clone)]
struct Projection {
    anchor: Option<(String, Location)>,
    key: Option<Key>,
    root: Option<BodyId>,
    order: usize,
    order_path: Vec<usize>,
    function: Function,
    semantic: SemanticFacts,
}
#[derive(Clone)]
struct Header {
    version: u32,
    compiler: String,
    compatibility: String,
    workspace: String,
    revision: String,
    sources: BTreeMap<String, String>,
}
impl Header {
    fn from(snapshot: &Snapshot) -> Self {
        Self {
            version: snapshot.version,
            compiler: snapshot.compiler.clone(),
            compatibility: snapshot.compatibility.clone(),
            workspace: snapshot.workspace.clone(),
            revision: snapshot.revision.clone(),
            sources: snapshot.sources.clone(),
        }
    }
    fn apply(&self, snapshot: &mut Snapshot) {
        snapshot.version = self.version;
        snapshot.compiler = self.compiler.clone();
        snapshot.compatibility = self.compatibility.clone();
        snapshot.workspace = self.workspace.clone();
        snapshot.revision = self.revision.clone();
        snapshot.sources = self.sources.clone();
    }
}
#[derive(Clone, Default)]
pub(crate) struct VerificationSnapshotState {
    metadata: Option<Arc<Snapshot>>,
    header: Option<Header>,
    records: im::OrdMap<String, Arc<Projection>>,
    roots: im::HashMap<BodyId, Root>,
    declarations: im::HashMap<(UnitId, String), Span>,
    units: im::HashMap<UnitId, Unit>,
    keys: im::HashMap<Key, String>,
    reverse: im::HashMap<String, im::OrdSet<String>>,
    own: im::HashMap<Key, u8>,
    bodyless: im::HashMap<(UnitId, String), bool>,
    encoded: im::HashMap<String, usize>,
    symbol_types: im::HashMap<String, Arc<Option<Type>>>,
    bytes: usize,
    owner_checks: usize,
    coordinate_owners: usize,
    anchors: im::HashMap<String, Location>,
    metadata_anchors: Arc<AnchorIndex>,
    indirect: super::incremental_indirect::State,
    dispatch: Option<super::incremental_dispatch::State>,
    dispatch_visits: usize,
    projection_items_cloned: std::cell::Cell<usize>,
    boundary_functions: std::cell::Cell<usize>,
    descendants: im::HashMap<BodyId, im::HashSet<String>>,
    instances: im::OrdMap<String, (String, Arc<Vec<String>>)>,
    instance_owners: im::HashMap<String, im::HashSet<String>>,
    instance_bounds: im::HashMap<String, usize>,
    instance_bytes: usize,
    dirty_instance_owners: HashSet<String>,
}
#[derive(Default)]
struct AnchorIndex(HashMap<String, Vec<(Location, String)>>);
impl AnchorIndex {
    fn new(anchors: &im::HashMap<String, Location>) -> Self {
        let mut paths: HashMap<String, Vec<(Location, String)>> = HashMap::new();
        for (key, location) in anchors {
            paths
                .entry(location.path.clone())
                .or_default()
                .push((location.clone(), key.clone()));
        }
        for values in paths.values_mut() {
            values.sort_by_key(|(location, _)| location.start);
        }
        Self(paths)
    }
    fn owner(&self, location: &Location) -> Option<(String, Location)> {
        let values = self.0.get(&location.path)?;
        let at = values
            .partition_point(|(anchor, _)| anchor.start <= location.start)
            .checked_sub(1)?;
        let (anchor, key) = &values[at];
        (location.end <= anchor.end).then(|| (key.clone(), anchor.clone()))
    }
}
fn anchor_map(old: &Location, new: &Location) -> RelocationBatch {
    RelocationBatch::new(&[VerificationRelocation {
        path: old.path.clone(),
        old_start: old.start,
        old_end: old.end,
        new_start: new.start,
        new_end: new.end,
    }])
}
fn order_path(
    index: &crate::compiler_db::ids::BodyIndex,
    body: Option<BodyId>,
    locations: &HashMap<BodyId, usize>,
) -> Vec<usize> {
    let mut path = Vec::new();
    let Some(mut body) = body else {
        return path;
    };
    while let Some((_, BodyOwner::Lambda { parent, .. })) = index.owner(body) {
        path.push(usize::MAX - locations.get(&body).copied().unwrap_or(0));
        body = parent;
    }
    path.reverse();
    path
}
fn indirect_inputs_tree(
    bodies: &BodyQueries,
    root: BodyId,
) -> Result<crate::semantic::indirect_lock::IndirectInputs> {
    let mut inputs = crate::semantic::indirect_lock::IndirectInputs::default();
    let mut work = vec![root];
    while let Some(body) = work.pop() {
        inputs.extend(bodies.indirect_inputs(body)?);
        work.extend(bodies.index().child_lambdas(body).map(|(_, child)| child));
    }
    Ok(inputs)
}
fn callable_owner(index: &crate::compiler_db::ids::BodyIndex, body: BodyId) -> Option<Key> {
    let (unit, owner) = index.owner(body)?;
    match owner {
        BodyOwner::Function(id) => Some((unit, id)),
        BodyOwner::InterfaceDefault(id) => Some((
            unit,
            FunctionId::method(id.owner_type()?, format!("$default${}", id.name())),
        )),
        BodyOwner::Lambda { .. } => Some((unit, FunctionId::lambda(body))),
        BodyOwner::Constructor { owner, .. } => Some((unit, FunctionId::method(owner, "init"))),
        _ => None,
    }
}
fn root_anchor(body: BodyId) -> String {
    format!("body:{body:?}")
}
fn declaration_anchor(key: &(UnitId, String)) -> String {
    format!("declaration:{:?}:{}", key.0, key.1)
}
#[derive(Default)]
struct RelocationBatch(HashMap<String, Vec<VerificationRelocation>>);
impl RelocationBatch {
    fn new(changes: &[VerificationRelocation]) -> Self {
        let mut result = Self::default();
        for change in changes {
            result
                .0
                .entry(change.path.clone())
                .or_default()
                .push(change.clone());
        }
        for ranges in result.0.values_mut() {
            ranges.sort_by_key(|r| r.old_start);
        }
        result
    }
    fn location(&self, location: &mut Location) {
        let Some(ranges) = self.0.get(&location.path) else {
            return;
        };
        let end = ranges.partition_point(|range| range.old_start <= location.start);
        let Some(map) = end.checked_sub(1).and_then(|i| ranges.get(i)) else {
            return;
        };
        if location.end > map.old_end {
            return;
        }
        if location.start == map.old_start && location.end == map.old_end {
            location.start = map.new_start;
            location.end = map.new_end;
        } else {
            let delta = map.new_start as isize - map.old_start as isize;
            location.start = location.start.saturating_add_signed(delta);
            location.end = location.end.saturating_add_signed(delta);
        }
    }
}
struct Plan {
    previous: VerificationSnapshotState,
    changed: HashSet<Key>,
    current_roots: HashMap<BodyId, Root>,
    current_keys: HashMap<Key, BodyId>,
    source_keys: HashMap<Key, HashSet<BodyId>>,
    constructor_groups: HashMap<Key, HashSet<BodyId>>,
    projected_bodies: HashSet<BodyId>,
    declarations: HashMap<(UnitId, String), Span>,
    pending_declarations: im::HashMap<(UnitId, String), Span>,
    bodyless: HashMap<(UnitId, String), bool>,
    removed: HashSet<Key>,
    annotation_starts: HashMap<UnitId, usize>,
    symbol_owners: HashMap<Span, String>,
    unit_items: HashMap<UnitId, usize>,
    fallback: bool,
    cold_fallback: bool,
    owner_checks: usize,
}
thread_local! { static PLAN: RefCell<Option<Rc<RefCell<Plan>>>> = const { RefCell::new(None) }; }
pub(crate) struct CaptureScope {
    previous: Option<Rc<RefCell<Plan>>>,
}
impl Drop for CaptureScope {
    fn drop(&mut self) {
        PLAN.with(|p| *p.borrow_mut() = self.previous.take());
    }
}
impl VerificationSnapshotState {
    pub(crate) fn projection_items_cloned(&self) -> usize {
        self.projection_items_cloned.get()
    }
    pub(crate) fn boundary_functions(&self) -> usize {
        self.boundary_functions.get()
    }

    fn enforce_size(&mut self) -> Result<()> {
        for owner in std::mem::take(&mut self.dirty_instance_owners) {
            self.instance_bytes -= self.instance_bounds.remove(&owner).unwrap_or(0);
            let Some(record) = self.records.get(&owner) else {
                continue;
            };
            let ids = self.instance_owners.get(&owner);
            let nodes = ids.map_or(1, |ids| ids.len().max(1));
            let raw_edges: usize = ids
                .into_iter()
                .flatten()
                .filter_map(|id| self.instances.get(id))
                .map(|(_, edges)| edges.len())
                .sum();
            // Numeric graph owners/edges use at most ten decimal digits. Each
            // source edge may add one edge per instance and one target union.
            let bound = 64usize.saturating_add(16usize.saturating_mul(raw_edges.saturating_add(
                nodes.saturating_mul(record.function.callees.len().saturating_add(3)),
            )));
            self.instance_bounds.insert(owner, bound);
            self.instance_bytes = self.instance_bytes.saturating_add(bound);
        }
        if self.bytes.saturating_add(self.instance_bytes) > 64 * 1024 * 1024 {
            ensure!(
                serde_json::to_vec(&self.materialize()?)?.len() <= 64 * 1024 * 1024,
                "verification snapshot exceeds 64 MiB"
            );
        }
        Ok(())
    }

    pub(crate) fn dispatch_visits(&self) -> usize {
        self.dispatch_visits
    }
    pub(crate) fn dispatch_nodes(&self) -> usize {
        self.dispatch.as_ref().map_or(0, |state| state.len())
    }

    pub(crate) fn owner_checks(&self) -> usize {
        self.owner_checks
    }
    pub(crate) fn coordinate_owners(&self) -> usize {
        self.coordinate_owners
    }

    pub(crate) fn capture_scope(&self) -> CaptureScope {
        let plan = self.metadata.as_ref().map(|_| {
            Rc::new(RefCell::new(Plan {
                previous: self.clone(),
                changed: HashSet::new(),
                current_roots: HashMap::new(),
                current_keys: HashMap::new(),
                source_keys: HashMap::new(),
                constructor_groups: HashMap::new(),
                projected_bodies: HashSet::new(),
                declarations: HashMap::new(),
                pending_declarations: self.declarations.clone(),
                bodyless: HashMap::new(),
                removed: HashSet::new(),
                annotation_starts: HashMap::new(),
                symbol_owners: HashMap::new(),
                unit_items: HashMap::new(),
                fallback: false,
                cold_fallback: false,
                owner_checks: 0,
            }))
        });
        CaptureScope {
            previous: PLAN.with(|slot| slot.replace(plan)),
        }
    }
    fn current_function(&self, id: &str) -> Function {
        let record = &self.records[id];
        let mut function = record.function.clone();
        if let Some((key, old)) = &record.anchor
            && let Some(current) = self.anchors.get(key)
        {
            let map = anchor_map(old, current);
            for location in &mut function.locations {
                map.location(location);
            }
            if let Some(location) = &mut function.body_location {
                map.location(location);
            }
        }
        function
    }
    fn boundary_function(&self, id: &str) -> Function {
        self.boundary_functions
            .set(self.boundary_functions.get() + 1);
        let record = &self.records[id];
        let f = &record.function;
        let mut function = Function {
            identity: f.identity.clone(),
            body_id: f.body_id,
            body_location: f.body_location.clone(),
            rename_calls: vec![],
            rename_imports: vec![],
            id: f.id.clone(),
            module: f.module.clone(),
            name: f.name.clone(),
            locations: f.locations.clone(),
            synthetic: f.synthetic,
            fingerprint: f.fingerprint.clone(),
            body_fingerprint: f.body_fingerprint.clone(),
            callees: vec![],
            runtime_effects: f.runtime_effects,
            unknown: f.unknown,
            unresolved: vec![],
        };
        if let Some((key, old)) = &record.anchor
            && let Some(current) = self.anchors.get(key)
        {
            let map = anchor_map(old, current);
            for location in &mut function.locations {
                map.location(location);
            }
            if let Some(location) = &mut function.body_location {
                map.location(location);
            }
        }
        function
    }
    fn current_record(&self, id: &str, measured: bool) -> Projection {
        if measured {
            let semantic = &self.records[id].semantic;
            let items = semantic.symbols.len()
                + semantic.references.len()
                + semantic.expressions.len()
                + semantic
                    .flows
                    .iter()
                    .map(|flow| flow.nodes.len())
                    .sum::<usize>()
                + semantic.witnesses.values().map(Vec::len).sum::<usize>()
                + semantic.effect_calls.values().map(Vec::len).sum::<usize>()
                + semantic
                    .unresolved_calls
                    .values()
                    .map(Vec::len)
                    .sum::<usize>();
            self.projection_items_cloned
                .set(self.projection_items_cloned.get() + items);
        }
        let mut record = (**self.records.get(id).expect("retained verification owner")).clone();
        if let Some((key, old)) = record.anchor.clone()
            && let Some(current) = self.anchors.get(&key)
        {
            let map = anchor_map(&old, current);
            relocate_projection(&mut record, &map);
            record.anchor = Some((key.clone(), current.clone()));
        }
        record
    }
    fn replace_instances(&mut self, entries: super::InstanceCapture, removed: &HashSet<String>) {
        let owners: HashSet<_> = entries
            .iter()
            .map(|(_, owner, _)| owner.clone())
            .chain(removed.iter().cloned())
            .collect();
        for owner in owners {
            self.dirty_instance_owners.insert(owner.clone());
            if let Some(ids) = self.instance_owners.remove(&owner) {
                for id in ids {
                    self.instances.remove(&id);
                }
            }
        }
        for (id, owner, callees) in entries {
            self.instance_owners
                .entry(owner.clone())
                .or_default()
                .insert(id.clone());
            self.instances.insert(id, (owner, Arc::new(callees)));
        }
    }
    pub(crate) fn materialize(&self) -> Result<Snapshot> {
        let mut result = (**self
            .metadata
            .as_ref()
            .context("missing verification snapshot")?)
        .clone();
        let relocate = |location: &mut Location| {
            if let Some((key, old)) = self.metadata_anchors.owner(location)
                && let Some(current) = self.anchors.get(&key)
            {
                anchor_map(&old, current).location(location);
            }
        };
        for symbol in &mut result.semantic.symbols {
            if let Some(location) = &mut symbol.location {
                relocate(location);
            }
        }
        for reference in &mut result.semantic.references {
            relocate(&mut reference.location);
        }
        if let Some(header) = &self.header {
            header.apply(&mut result);
        }
        let mut records: Vec<_> = self.records.keys().collect();
        records.sort_by_key(|id| {
            let record = &self.records[*id];
            let root = record.root.and_then(|body| self.roots.get(&body));
            let (kind, member) = root.map_or((usize::MAX, 0), |root| match root.kind {
                RootKind::Function => (0, 0),
                RootKind::Constructor(member) => (0, member),
                RootKind::Method(member) | RootKind::Default(member) => (1, member),
            });
            (
                record.key.map(|key| key.0.0).unwrap_or(u32::MAX),
                std::cmp::Reverse(root.map_or(0, |root| root.item)),
                kind,
                std::cmp::Reverse(member),
                record.order_path.clone(),
                record.order,
            )
        });
        for id in records {
            let projection = self.current_record(id, false);
            result.functions.push(projection.function);
            merge_semantic(&mut result.semantic, projection.semantic);
        }
        result.functions.sort_by(|a, b| a.id.cmp(&b.id));
        let ids: Vec<_> = self.instances.keys().cloned().collect();
        let callees: Vec<_> = self
            .instances
            .values()
            .map(|(_, callees)| (**callees).clone())
            .collect();
        let merged: HashMap<_, _> = self
            .instances
            .iter()
            .filter(|(id, (owner, _))| *id != owner)
            .map(|(id, (owner, _))| (id.clone(), owner.clone()))
            .collect();
        result.semantic.instances =
            super::graph::Instances::new(&ids, &callees, &merged, &result.functions);

        result.semantic.symbols.sort_by(|a, b| a.id.cmp(&b.id));
        result.semantic.symbols.dedup_by(|a, b| a.id == b.id);
        result.semantic.references.sort_by(|a, b| {
            (
                &a.location.path,
                a.location.start,
                a.location.end,
                &a.target,
                &a.role,
            )
                .cmp(&(
                    &b.location.path,
                    b.location.start,
                    b.location.end,
                    &b.target,
                    &b.role,
                ))
        });
        result.revision = result.digest()?;
        Ok(result)
    }
}

pub(crate) fn record_annotation_start(unit: UnitId, start: usize) {
    PLAN.with(|slot| {
        if let Some(plan) = slot.borrow().as_ref() {
            plan.borrow_mut().annotation_starts.insert(unit, start);
        }
    });
}
pub(crate) fn record_declaration(unit: UnitId, item: &Item) {
    let declaration = match item {
        Item::Class(value) => Some((format!("class:{}", value.name), value.span)),
        Item::Interface(value) => Some((format!("interface:{}", value.name), value.span)),
        Item::Enum(value) => Some((format!("enum:{}", value.name), value.span)),
        _ => None,
    };
    if let Some((name, span)) = declaration {
        PLAN.with(|slot| {
            if let Some(plan) = slot.borrow().as_ref() {
                let mut plan = plan.borrow_mut();
                plan.symbol_owners
                    .extend(symbols::owners_of(std::iter::once(item)));
                let key = (unit, name);
                plan.pending_declarations.remove(&key);
                plan.declarations.insert(key, span);
                match item {
                    Item::Class(class) if class.constructors.is_empty() => {
                        plan.bodyless.insert((unit, class.name.clone()), false);
                    }
                    Item::Enum(enumeration) => {
                        plan.bodyless.insert((unit, enumeration.name.clone()), true);
                    }
                    _ => {}
                }
            }
        });
    }
}
pub(crate) fn record_root(unit: UnitId, item: usize, function: &FunctionDecl) {
    PLAN.with(|slot| {
        if let Some(plan) = slot.borrow().as_ref() {
            let mut plan = plan.borrow_mut();
            let source = generics::source_spelling(&function.name);
            plan.source_keys
                .entry((unit, FunctionId::free(&source)))
                .or_default()
                .insert(function.body.id);
            if plan
                .current_keys
                .insert((unit, FunctionId::free(&function.name)), function.body.id)
                .is_some()
            {
                plan.fallback = true;
                plan.cold_fallback = true;
            }
            plan.current_roots.insert(
                function.body.id,
                Root {
                    item,
                    kind: RootKind::Function,
                    span: function.span,
                    key: (unit, FunctionId::free(&function.name)),
                },
            );
        }
    });
}
pub(crate) fn record_method(
    unit: UnitId,
    item: usize,
    member: usize,
    class: &ClassDecl,
    method: &crate::parser::ast::MethodDecl,
) {
    PLAN.with(|slot| {
        if let Some(plan) = slot.borrow().as_ref() {
            let mut plan = plan.borrow_mut();
            let key = (
                unit,
                FunctionId::method(
                    crate::semantic::ids::TypeId::local(&class.name),
                    &method.name,
                ),
            );
            if plan.current_keys.insert(key, method.body.id).is_some() {
                plan.fallback = true;
                plan.cold_fallback = true;
            }
            plan.current_roots.insert(
                method.body.id,
                Root {
                    item,
                    kind: RootKind::Method(member),
                    span: method.span,
                    key,
                },
            );
        }
    });
}
pub(crate) fn record_constructor(
    unit: UnitId,
    item: usize,
    member: usize,
    class: &ClassDecl,
    constructor: &crate::parser::ast::ConstructorDecl,
) {
    record_member_root(
        unit,
        item,
        RootKind::Constructor(member),
        constructor.body.id,
        constructor.span,
        FunctionId::method(TypeId::local(&class.name), "init"),
    );
}
pub(crate) fn record_default(
    unit: UnitId,
    item: usize,
    member: usize,
    interface: &crate::parser::ast::InterfaceDecl,
    method: &crate::parser::ast::InterfaceMethodDecl,
) {
    if let Some(body) = &method.default_body {
        record_member_root(
            unit,
            item,
            RootKind::Default(member),
            body.id,
            method.span,
            FunctionId::method(
                TypeId::local(&interface.name),
                format!("$default${}", method.name),
            ),
        );
    }
}
fn record_member_root(
    unit: UnitId,
    item: usize,
    kind: RootKind,
    body: BodyId,
    span: Span,
    id: FunctionId,
) {
    PLAN.with(|slot| {
        if let Some(plan) = slot.borrow().as_ref() {
            let mut plan = plan.borrow_mut();
            let key = (unit, id);
            if matches!(kind, RootKind::Constructor(_)) {
                plan.current_keys.entry(key).or_insert(body);
                plan.constructor_groups.entry(key).or_default().insert(body);
            } else if plan.current_keys.insert(key, body).is_some() {
                plan.fallback = true;
                plan.cold_fallback = true;
            }
            plan.current_roots.insert(
                body,
                Root {
                    item,
                    kind,
                    span,
                    key,
                },
            );
        }
    });
}
fn imports(program: &Program) -> String {
    hash_serialized(&crate::compiler_db::syntax::semantic(&serde_json::json!([
        program.module,
        program.imports,
        program.instantiation_types
    ])))
    .expect("unit imports")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn capture_incremental(
    unit: UnitId,
    bodies: Option<&BodyQueries>,
    queries: &EffectQueries,
    program: &Program,
    graph: &CallGraph,
    inputs: CaptureInputs<'_>,
    facts: &crate::semantic::effects::EffectFacts<crate::compiler_db::effects::EffectWitness>,
    symbols: &crate::semantic::symbols::SymbolTable,
    index: Option<&crate::compiler_db::ids::BodyIndex>,
) -> CapturedUnit {
    let plan = PLAN.with(|slot| slot.borrow().clone());
    let Some(plan) = plan else {
        return capture(program, graph, inputs, facts, symbols, index);
    };
    let Some(bodies) = bodies else {
        plan.borrow_mut().fallback = true;
        return capture(program, graph, inputs, facts, symbols, index);
    };
    let selection = (|| -> Result<_> {
        let mut plan = plan.borrow_mut();

        let known = plan
            .previous
            .units
            .get(&unit)
            .ok_or(VerificationSnapshotFallback)?;
        if !known.unique_roots {
            plan.cold_fallback = true;
            return Err(VerificationSnapshotColdFallback.into());
        }
        ensure!(
            known.imports == imports(program),
            VerificationSnapshotFallback
        );
        let source_changes = bodies
            .effect_source_changes(unit)
            .ok_or(VerificationSnapshotFallback)?;
        if known.items != program.items.len() || bodies.declarations_changed(unit) {
            ensure!(
                source_changes
                    .iter()
                    .all(|(owner, _)| owner.starts_with("Function:")),
                VerificationSnapshotFallback
            );
        }
        plan.unit_items.insert(unit, program.items.len());
        let mut changed: HashSet<_> = bodies
            .changed_bodies_in(unit)
            .into_iter()
            .map(|body| bodies.index().root_body(body))
            .collect();
        let mut source_only_spans = HashSet::new();
        for (owner, exists) in source_changes {
            plan.owner_checks += 1;
            let name = owner
                .strip_prefix("Function:")
                .ok_or(VerificationSnapshotFallback)?;
            let key = (unit, FunctionId::free(name));
            if exists {
                if let Some(group) = plan.source_keys.get(&key) {
                    changed.extend(group.iter().copied());
                } else if let Some(span) = bodies.source_owner_span(unit, &owner) {
                    plan.changed.insert(key);
                    source_only_spans.insert(span);
                } else {
                    return Err(VerificationSnapshotFallback.into());
                }
            } else {
                plan.removed.insert(key);
            }
        }
        for owner in bodies.source_body_changes(unit) {
            let Some(name) = owner.strip_prefix("Function:") else {
                continue;
            };
            let key = (unit, FunctionId::free(name));
            if !plan.source_keys.contains_key(&key)
                && let Some(span) = bodies.source_owner_span(unit, &owner)
            {
                plan.owner_checks += 1;
                plan.changed.insert(key);
                source_only_spans.insert(span);
            }
        }
        let mut selected = Program {
            instantiation_types: vec![],
            type_uses: vec![],
            module: program.module.clone(),
            imports: vec![],
            items: vec![],
        };
        let mut selected_graph = CallGraph::default();
        let mut selected_types = HashMap::new();
        let mut selected_calls = HashMap::new();
        let mut selected_patterns = HashMap::new();
        let mut constructor_items = HashMap::<usize, usize>::new();
        let mut selected_symbols = crate::semantic::analysis_symbols::Facts::default();
        let mut selected_indirect = crate::semantic::indirect_lock::IndirectInputs::default();
        // A retired instance can leave a source template with no checked body.
        // Select current source coordinates, including edits before the owner.
        for body in queries.removed_roots(unit) {
            if let Some(root) = plan.previous.roots.get(&body)
                && matches!(root.kind, RootKind::Function)
                && let Some(span) = bodies.source_owner_span(
                    unit,
                    &format!("Function:{}", generics::source_spelling(&root.key.1.name())),
                )
            {
                source_only_spans.insert(span);
            }
        }
        if let Some(&start) = plan.annotation_starts.get(&unit) {
            let annotations = &inputs.symbols.references[start..];
            for span in source_only_spans {
                let first = annotations.partition_point(|r| r.span.start < span.start);
                let end = annotations.partition_point(|r| r.span.start < span.end);
                selected_symbols.references.extend(
                    annotations[first..end]
                        .iter()
                        .filter(|r| r.span.end <= span.end)
                        .cloned(),
                );
            }
        }

        // A dependent body's compiler evidence can change without a typed
        // body change (for example an imported callee's panic boundary).
        // Reproject affected owners through the same witness precedence as
        // cold capture, reusing their retained typed bodies and symbol facts.
        for function in queries.affected_functions(unit) {
            if let Some(body) = plan.current_keys.get(&(unit, function)) {
                changed.insert(*body);
            }
        }
        // A source template is one public projection even when several checked
        // instances contribute to it. Reconstruct that affected projection from
        // its complete current contribution group on additions and removals.
        let generic_groups: HashSet<_> = changed
            .iter()
            .chain(queries.removed_roots(unit).iter())
            .filter_map(|body| {
                plan.current_roots
                    .get(body)
                    .or_else(|| plan.previous.roots.get(body))
            })
            .filter(|root| matches!(root.kind, RootKind::Function))
            .map(|root| {
                (
                    unit,
                    FunctionId::free(generics::source_spelling(&root.key.1.name())),
                )
            })
            .collect();
        let related_instances: Vec<_> = generic_groups
            .iter()
            .filter_map(|key| plan.source_keys.get(key))
            .flat_map(|group| group.iter().copied())
            .collect();
        changed.extend(related_instances);
        let related: Vec<_> = changed
            .iter()
            .filter_map(|body| plan.current_roots.get(body))
            .filter_map(|root| plan.constructor_groups.get(&root.key))
            .flat_map(|group| group.iter().copied())
            .collect();
        changed.extend(related);
        let mut changed: Vec<_> = changed.into_iter().collect();
        changed.sort_by_key(|body| {
            plan.current_roots.get(body).map(|root| {
                (
                    root.item,
                    match root.kind {
                        RootKind::Function => (0, 0),
                        RootKind::Method(member) => (1, member),
                        RootKind::Constructor(member) => (2, member),
                        RootKind::Default(member) => (1, member),
                    },
                )
            })
        });
        for body in changed {
            plan.owner_checks += 1;
            let owner_unit = plan
                .current_roots
                .get(&body)
                .map(|root| root.key.0)
                .or_else(|| bodies.index().owner(body).map(|(unit, _)| unit));
            if owner_unit != Some(unit) {
                continue;
            }
            let root = plan
                .current_roots
                .get(&body)
                .ok_or(VerificationSnapshotFallback)?
                .clone();
            let (block, span, item) = match (program.items.get(root.item), root.kind) {
                (Some(Item::Function(function)), RootKind::Function) => {
                    ensure!(
                        function.type_params.is_empty() && function.rust_bridge.is_none(),
                        VerificationSnapshotFallback
                    );
                    (
                        &function.body,
                        function.span,
                        Item::Function(function.clone()),
                    )
                }
                (Some(Item::Class(class)), RootKind::Method(member)) => {
                    ensure!(class.type_params.is_empty(), VerificationSnapshotFallback);
                    let method = class
                        .methods
                        .get(member)
                        .ok_or(VerificationSnapshotFallback)?;
                    let selected_class = ClassDecl {
                        abi_storage: class.abi_storage.clone(),
                        type_params: vec![],
                        name: class.name.clone(),
                        public: class.public,
                        is_open: class.is_open,
                        base_class: class.base_class.clone(),
                        implements: class.implements.clone(),
                        source_implements_len: class.source_implements_len,
                        fields: vec![],
                        methods: vec![method.clone()],
                        constructors: vec![],
                        span: class.span,
                    };
                    (&method.body, method.span, Item::Class(selected_class))
                }
                (Some(Item::Class(class)), RootKind::Constructor(member)) => {
                    ensure!(class.type_params.is_empty(), VerificationSnapshotFallback);
                    let constructor = class
                        .constructors
                        .get(member)
                        .ok_or(VerificationSnapshotFallback)?;
                    let selected_class = ClassDecl {
                        abi_storage: class.abi_storage.clone(),
                        type_params: vec![],
                        name: class.name.clone(),
                        public: class.public,
                        is_open: class.is_open,
                        base_class: class.base_class.clone(),
                        implements: class.implements.clone(),
                        source_implements_len: class.source_implements_len,
                        fields: vec![],
                        methods: vec![],
                        constructors: vec![constructor.clone()],
                        span: class.span,
                    };
                    (
                        &constructor.body,
                        constructor.span,
                        Item::Class(selected_class),
                    )
                }
                (Some(Item::Interface(interface)), RootKind::Default(member)) => {
                    ensure!(
                        interface.type_params.is_empty(),
                        VerificationSnapshotFallback
                    );
                    let method = interface
                        .methods
                        .get(member)
                        .ok_or(VerificationSnapshotFallback)?;
                    let block = method
                        .default_body
                        .as_ref()
                        .ok_or(VerificationSnapshotFallback)?;
                    let selected_interface = crate::parser::ast::InterfaceDecl {
                        default_import_uses: vec![],
                        name: interface.name.clone(),
                        public: interface.public,
                        type_params: vec![],
                        extends: interface.extends.clone(),
                        methods: vec![method.clone()],
                        instance_of: interface.instance_of.clone(),
                        span: interface.span,
                    };
                    (block, method.span, Item::Interface(selected_interface))
                }
                _ => return Err(VerificationSnapshotFallback.into()),
            };
            ensure!(block.id == body, VerificationSnapshotFallback);
            if let Some(sites) = graph.get(&root.key.1) {
                selected_graph.merge(root.key.1, sites.clone());
            }
            let mut nested = vec![body];
            while let Some(parent) = nested.pop() {
                for (_, child) in bodies.index().child_lambdas(parent) {
                    if let Some(sites) = graph.get(&FunctionId::lambda(child)) {
                        selected_graph.merge(FunctionId::lambda(child), sites.clone());
                    }
                    let typed = bodies.read(child)?;
                    selected_symbols.extend(typed.analysis_symbols());
                    typed.snapshot_tables(
                        &mut selected_types,
                        &mut selected_calls,
                        &mut selected_patterns,
                    );
                    selected_indirect.extend(bodies.indirect_inputs(child)?);
                    nested.push(child);
                }
            }
            let copied = matches!((&item,root.kind),(Item::Class(class),RootKind::Method(_)) if class.methods.iter().any(|method|method.is_interface_default));
            match bodies.indirect_inputs(body) {
                Ok(inputs) => selected_indirect.extend(inputs),
                Err(_) if copied => {}
                Err(error) => return Err(error),
            }
            match bodies.read(body) {
                Ok(typed) => {
                    selected_symbols.extend(typed.analysis_symbols());
                    if !copied {
                        typed.snapshot_tables(
                            &mut selected_types,
                            &mut selected_calls,
                            &mut selected_patterns,
                        );
                    }
                }
                Err(_) if copied => {}
                Err(error) => return Err(error),
            }
            // Copied defaults may use checked metadata owned by their source.
            for event in AstWalk::new(AstEvent::Block(block)) {
                if let AstEvent::Expr(expression) = event {
                    let id = expression.id();
                    if let Some(value) = inputs.types.get(&id) {
                        selected_types.insert(id, value.clone());
                    }
                    if let Some(value) = inputs.calls.get(&id) {
                        selected_calls.insert(id, *value);
                    }
                }
            }
            if let Some(&start) = plan.annotation_starts.get(&unit) {
                let annotations = &inputs.symbols.references[start..];
                let first = annotations.partition_point(|r| r.span.start < span.start);
                let end = annotations.partition_point(|r| r.span.start < span.end);
                selected_symbols.references.extend(
                    annotations[first..end]
                        .iter()
                        .filter(|r| r.span.end <= span.end)
                        .cloned(),
                );
            }
            if matches!(root.kind, RootKind::Constructor(_)) {
                if let Some(&index) = constructor_items.get(&root.item) {
                    if let (Item::Class(existing), Item::Class(next)) =
                        (&mut selected.items[index], item)
                    {
                        existing.constructors.extend(next.constructors);
                    }
                } else {
                    constructor_items.insert(root.item, selected.items.len());
                    selected.items.push(item);
                }
            } else {
                selected.items.push(item);
            }
            plan.projected_bodies.insert(body);
            plan.changed.insert(root.key);
        }
        if let Some(known) = plan.previous.units.get(&unit) {
            selected.imports = selected_symbols
                .used_imports
                .iter()
                .filter_map(|name| known.import_names.get(name).cloned())
                .collect();
            selected.imports.sort_by_key(|import| import.span.start);
        }
        Ok((
            selected,
            selected_graph,
            selected_symbols,
            selected_indirect,
            selected_types,
            selected_calls,
            selected_patterns,
        ))
    })();
    match selection {
        Ok((
            selected,
            selected_graph,
            selected_symbols,
            selected_indirect,
            selected_types,
            selected_calls,
            selected_patterns,
        )) => {
            let mut captured = capture(
                &selected,
                &selected_graph,
                CaptureInputs {
                    indirect: &selected_indirect,
                    types: &selected_types,
                    calls: &selected_calls,
                    patterns: &selected_patterns,
                    symbols: &selected_symbols,
                },
                facts,
                symbols,
                index,
            );
            let mut owners = HashSet::new();
            let plan = plan.borrow();
            for reference in &selected_symbols.references {
                let target = &reference.target;
                if owners.insert(target.span) {
                    if let Some(owner) = plan.symbol_owners.get(&target.span) {
                        captured.symbol_owners.push((target.span, owner.clone()));
                    } else if matches!(target.kind.as_str(), "class" | "interface" | "enum") {
                        captured.symbol_owners.push((
                            target.span,
                            target
                                .name
                                .rsplit("::")
                                .next()
                                .unwrap_or(&target.name)
                                .to_owned(),
                        ));
                    }
                }
            }
            captured
        }
        Err(_) => {
            plan.borrow_mut().fallback = true;
            capture(program, graph, inputs, facts, symbols, index)
        }
    }
}

fn merge_semantic(target: &mut SemanticFacts, mut source: SemanticFacts) {
    let base = target.flows.len();
    for expression in &mut source.expressions {
        expression.flow += base;
    }
    target.symbols.extend(source.symbols);
    target.references.extend(source.references);
    target.expressions.extend(source.expressions);
    target.flows.extend(source.flows);
    target.witnesses.extend(source.witnesses);
    target.compiler_effects.extend(source.compiler_effects);
    target.effect_calls.extend(source.effect_calls);
    target.async_functions.extend(source.async_functions);
    target.unresolved_calls.extend(source.unresolved_calls);
}
fn owner_index(snapshot: &Snapshot) -> HashMap<(String, usize, usize), String> {
    let mut ranges: HashMap<&str, Vec<(usize, usize, &str)>> = HashMap::new();
    for f in &snapshot.functions {
        for l in &f.locations {
            ranges
                .entry(&l.path)
                .or_default()
                .push((l.start, l.end, &f.id));
        }
    }
    let mut points: HashMap<&str, Vec<&Location>> = HashMap::new();
    for l in snapshot
        .semantic
        .symbols
        .iter()
        .filter_map(|s| s.location.as_ref())
        .chain(snapshot.semantic.references.iter().map(|r| &r.location))
    {
        points.entry(&l.path).or_default().push(l);
    }
    let mut result = HashMap::new();
    for (path, mut points) in points {
        let Some(ranges) = ranges.get_mut(path) else {
            continue;
        };
        ranges.sort_by_key(|(s, e, _)| (*s, std::cmp::Reverse(*e)));
        points.sort_by_key(|l| l.start);
        let mut stack: Vec<(usize, usize, &str)> = Vec::new();
        let mut next = 0;
        for l in points {
            while next < ranges.len() && ranges[next].0 <= l.start {
                let range = ranges[next];
                next += 1;
                while stack.last().is_some_and(|r| r.1 <= range.0) {
                    stack.pop();
                }
                stack.push(range);
            }
            while stack.last().is_some_and(|r| r.1 < l.end) {
                stack.pop();
            }
            if let Some((_, _, id)) = stack.last() {
                result.insert((path.to_owned(), l.start, l.end), (*id).to_owned());
            }
        }
    }
    result
}
fn split(snapshot: &Snapshot) -> (Snapshot, HashMap<String, Projection>) {
    let mut metadata = snapshot.clone();
    metadata.functions.clear();
    let mut records: HashMap<_, _> = snapshot
        .functions
        .iter()
        .map(|f| {
            (
                f.id.clone(),
                Projection {
                    anchor: None,
                    key: None,
                    root: None,
                    order: usize::MAX,
                    order_path: Vec::new(),
                    function: f.clone(),
                    semantic: SemanticFacts::default(),
                },
            )
        })
        .collect();
    let owners = owner_index(snapshot);
    let mut builtin_owners: HashMap<&str, HashSet<&str>> = HashMap::new();
    for reference in &snapshot.semantic.references {
        if reference.target.starts_with("builtin:")
            && let Some(owner) = owners.get(&(
                reference.location.path.clone(),
                reference.location.start,
                reference.location.end,
            ))
        {
            builtin_owners
                .entry(&reference.target)
                .or_default()
                .insert(owner);
        }
    }
    metadata.semantic.symbols.retain(|s| {
        if let Some(owners) = builtin_owners.get(s.id.as_str()) {
            for owner in owners {
                records
                    .get_mut(*owner)
                    .expect("indexed owner")
                    .semantic
                    .symbols
                    .push(s.clone());
            }
            return false;
        }

        let owner = records
            .contains_key(&s.id)
            .then_some(s.id.as_str())
            .or_else(|| {
                s.location.as_ref().and_then(|l| {
                    owners
                        .get(&(l.path.clone(), l.start, l.end))
                        .map(String::as_str)
                })
            });
        if let Some(owner) = owner {
            records
                .get_mut(owner)
                .unwrap()
                .semantic
                .symbols
                .push(s.clone());
            false
        } else {
            true
        }
    });
    metadata.semantic.references.retain(|r| {
        if let Some(owner) = owners
            .get(&(r.location.path.clone(), r.location.start, r.location.end))
            .map(String::as_str)
        {
            records
                .get_mut(owner)
                .unwrap()
                .semantic
                .references
                .push(r.clone());
            false
        } else {
            true
        }
    });
    let mut flow_indices = HashMap::new();
    for (i, flow) in snapshot.semantic.flows.iter().enumerate() {
        if let Some(record) = records.get_mut(&flow.function) {
            record.order = record.order.min(i);
            flow_indices.insert(i, record.semantic.flows.len());
            record.semantic.flows.push(flow.clone());
        }
    }
    metadata.semantic.flows.clear();
    for expression in &snapshot.semantic.expressions {
        if let Some(record) = records.get_mut(&expression.function) {
            let mut expression = expression.clone();
            expression.flow = flow_indices[&expression.flow];
            record.semantic.expressions.push(expression);
        }
    }
    metadata.semantic.expressions.clear();
    for (id, record) in &mut records {
        if let Some(value) = metadata.semantic.witnesses.remove(id) {
            record.semantic.witnesses.insert(id.clone(), value);
        }
        if let Some(value) = metadata.semantic.compiler_effects.remove(id) {
            record.semantic.compiler_effects.insert(id.clone(), value);
        }
        if let Some(value) = metadata.semantic.effect_calls.remove(id) {
            record.semantic.effect_calls.insert(id.clone(), value);
        }
        if let Some(value) = metadata.semantic.unresolved_calls.remove(id) {
            record.semantic.unresolved_calls.insert(id.clone(), value);
        }
        if metadata.semantic.async_functions.remove(id) {
            record.semantic.async_functions.insert(id.clone());
        }
    }
    metadata.semantic.instances = Default::default();
    (metadata, records)
}
// Reserve the maximum decimal width of every coordinate when a projection is
// produced. Lazy source relocation can then never grow its encoded size budget.
fn bounded_encoded(value: &serde_json::Value) -> Result<usize> {
    fn coordinate_slack(value: &serde_json::Value) -> usize {
        match value {
            serde_json::Value::Object(fields) => fields
                .iter()
                .map(|(key, value)| {
                    if matches!(key.as_str(), "start" | "end") && value.is_u64() {
                        20usize.saturating_sub(value.to_string().len())
                    } else {
                        coordinate_slack(value)
                    }
                })
                .sum(),
            serde_json::Value::Array(values) => values.iter().map(coordinate_slack).sum(),
            _ => 0,
        }
    }
    Ok(serde_json::to_vec(value)?.len() + coordinate_slack(value))
}
fn encoded(record: &Projection) -> Result<usize> {
    bounded_encoded(&serde_json::to_value((&record.function, &record.semantic))?)
}
fn relocate_value(value: &mut serde_json::Value, maps: &RelocationBatch) {
    if value.get("path").is_some()
        && value.get("start").is_some()
        && value.get("end").is_some()
        && let Ok(mut location) = serde_json::from_value::<Location>(value.clone())
    {
        maps.location(&mut location);
        *value = serde_json::to_value(location).unwrap();
        return;
    }
    match value {
        serde_json::Value::Array(a) => {
            for v in a {
                relocate_value(v, maps)
            }
        }
        serde_json::Value::Object(o) => {
            for v in o.values_mut() {
                relocate_value(v, maps)
            }
        }
        _ => {}
    }
}
fn relocate_projection(record: &mut Projection, maps: &RelocationBatch) {
    for l in &mut record.function.locations {
        maps.location(l)
    }
    if let Some(l) = &mut record.function.body_location {
        maps.location(l)
    }
    for s in &mut record.semantic.symbols {
        if let Some(l) = &mut s.location {
            maps.location(l)
        }
    }
    for r in &mut record.semantic.references {
        maps.location(&mut r.location)
    }
    for e in &mut record.semantic.expressions {
        maps.location(&mut e.location)
    }
    for f in &mut record.semantic.flows {
        for n in &mut f.nodes {
            maps.location(&mut n.location)
        }
    }
    for ws in record.semantic.witnesses.values_mut() {
        for w in ws {
            relocate_value(w, maps);
        }
    }
    for cs in record.semantic.effect_calls.values_mut() {
        for c in cs {
            maps.location(&mut c.location)
        }
    }
    for cs in record.semantic.unresolved_calls.values_mut() {
        for c in cs {
            maps.location(&mut c.location)
        }
    }
}

pub(crate) fn incremental_snapshot(
    frontend: &crate::Frontend,
    entry: &Path,
    source: &str,
    project: Option<&Path>,
    state: &mut VerificationSnapshotState,
    _changed: &HashSet<BodyId>,
) -> Result<VerificationSnapshotUpdate> {
    let plan = PLAN.with(|slot| slot.borrow().clone());
    let own: Vec<_> = frontend
        .db
        .effects
        .analysis
        .borrow()
        .as_ref()
        .into_iter()
        .flat_map(|units| units.iter())
        .flat_map(|(&unit, c)| {
            c.own_effects
                .iter()
                .map(move |(&id, &bits)| ((unit, id), bits))
        })
        .collect();
    if state.metadata.is_none() || plan.is_none() {
        let graph_keys: Vec<_> = frontend
            .db
            .effects
            .analysis
            .borrow()
            .as_ref()
            .into_iter()
            .flat_map(|captures| captures.iter())
            .flat_map(|(&unit, capture)| capture.graph.ids().map(move |id| (unit, *id)))
            .collect();
        let mut indirect = super::incremental_indirect::State::default();
        if let Some(captures) = frontend.db.effects.analysis.borrow().as_ref() {
            indirect.seed(captures);
        }
        let dispatch = RefCell::new(None);
        let instances = RefCell::new(Vec::new());
        let full = snapshot_with_boundary(
            frontend,
            entry,
            source,
            project,
            SnapshotBoundary {
                instances: Some(&instances),
                dispatch: Some(&dispatch),
                ..SnapshotBoundary::default()
            },
        )?;
        let (metadata, records) = split(&full);
        *state = VerificationSnapshotState::default();
        state.dispatch = dispatch.into_inner();
        state.indirect = indirect;
        state.replace_instances(instances.into_inner(), &HashSet::new());
        state.bytes = bounded_encoded(&serde_json::to_value(&metadata)?)?;
        state.header = Some(Header::from(&full));
        for symbol in &full.semantic.symbols {
            if !symbol.id.starts_with("builtin:") {
                state
                    .symbol_types
                    .insert(symbol.id.clone(), Arc::new(symbol.ty.clone()));
            }
        }
        state.metadata = Some(Arc::new(metadata));
        for (id, record) in records {
            let size = encoded(&record)?;
            state.bytes += size;
            state.encoded.insert(id.clone(), size);
            if let Some(body) = record.function.body_id {
                state
                    .descendants
                    .entry(frontend.db.bodies().root_body(body))
                    .or_default()
                    .insert(id.clone());
            }
            state.dirty_instance_owners.insert(id.clone());
            state.records.insert(id, Arc::new(record));
        }
        for (key, bits) in own {
            state.own.insert(key, bits);
        }
        for (unit, program, path) in std::iter::once((
            UnitId::ENTRY,
            &frontend.program,
            entry.to_string_lossy().into_owned(),
        ))
        .chain(
            frontend
                .module_graph
                .files
                .iter()
                .map(|m| (m.id, &m.program, m.path.to_string_lossy().into_owned())),
        ) {
            state.units.insert(
                unit,
                Unit {
                    unique_roots: true,
                    items: program.items.len(),
                    imports: imports(program),
                    import_names: Arc::new(
                        program
                            .imports
                            .iter()
                            .map(|import| {
                                (
                                    import.alias.clone().unwrap_or_else(|| {
                                        import
                                            .path
                                            .rsplit("::")
                                            .next()
                                            .unwrap_or(&import.path)
                                            .to_owned()
                                    }),
                                    import.clone(),
                                )
                            })
                            .collect(),
                    ),
                    path,
                },
            );
            let mut root_names = HashSet::new();
            for (item, node) in program.items.iter().enumerate() {
                let declaration = match node {
                    Item::Class(value) => Some((format!("class:{}", value.name), value.span)),
                    Item::Interface(value) => {
                        Some((format!("interface:{}", value.name), value.span))
                    }
                    Item::Enum(value) => Some((format!("enum:{}", value.name), value.span)),
                    _ => None,
                };
                if let Some((name, span)) = declaration {
                    state.declarations.insert((unit, name), span);
                }
                match node {
                    Item::Class(c) => {
                        for (member, method) in c.methods.iter().enumerate() {
                            state.roots.insert(
                                method.body.id,
                                Root {
                                    item,
                                    kind: RootKind::Method(member),
                                    span: method.span,
                                    key: (
                                        unit,
                                        FunctionId::method(
                                            crate::semantic::ids::TypeId::local(&c.name),
                                            &method.name,
                                        ),
                                    ),
                                },
                            );
                        }
                        for (member, constructor) in c.constructors.iter().enumerate() {
                            state.roots.insert(
                                constructor.body.id,
                                Root {
                                    item,
                                    kind: RootKind::Constructor(member),
                                    span: constructor.span,
                                    key: (unit, FunctionId::method(TypeId::local(&c.name), "init")),
                                },
                            );
                        }
                        if c.constructors.is_empty() {
                            state.bodyless.insert((unit, c.name.clone()), false);
                        }
                    }
                    Item::Interface(interface) => {
                        for (member, method) in interface.methods.iter().enumerate() {
                            if let Some(body) = &method.default_body {
                                state.roots.insert(
                                    body.id,
                                    Root {
                                        item,
                                        kind: RootKind::Default(member),
                                        span: method.span,
                                        key: (
                                            unit,
                                            FunctionId::method(
                                                TypeId::local(&interface.name),
                                                format!("$default${}", method.name),
                                            ),
                                        ),
                                    },
                                );
                            }
                        }
                    }
                    Item::Enum(e) => {
                        state.bodyless.insert((unit, e.name.clone()), true);
                    }
                    _ => {}
                }
                if let Item::Function(f) = node {
                    if !root_names.insert(&f.name) {
                        state
                            .units
                            .get_mut(&unit)
                            .expect("registered unit")
                            .unique_roots = false;
                    }
                    state.roots.insert(
                        f.body.id,
                        Root {
                            item,
                            kind: RootKind::Function,
                            span: f.span,
                            key: (unit, FunctionId::free(&f.name)),
                        },
                    );
                }
            }
        }
        for (body, root) in &state.roots {
            state.anchors.insert(
                root_anchor(*body),
                Location {
                    path: state.units[&root.key.0].path.clone(),
                    start: root.span.start,
                    end: root.span.end,
                },
            );
        }
        for (key, span) in &state.declarations {
            state.anchors.insert(
                declaration_anchor(key),
                Location {
                    path: state.units[&key.0].path.clone(),
                    start: span.start,
                    end: span.end,
                },
            );
        }
        state.metadata_anchors = Arc::new(AnchorIndex::new(&state.anchors));
        let body_locations: HashMap<_, _> = full
            .functions
            .iter()
            .filter_map(|function| Some((function.body_id?, function.locations.first()?.start)))
            .collect();
        let record_ids: Vec<_> = state.records.keys().cloned().collect();
        for id in record_ids {
            let record = Arc::make_mut(state.records.get_mut(&id).expect("retained owner"));
            record.order_path = order_path(
                frontend.db.bodies(),
                record.function.body_id,
                &body_locations,
            );
            record.key = record
                .function
                .body_id
                .and_then(|body| callable_owner(frontend.db.bodies(), body));
            record.root = record
                .function
                .body_id
                .map(|body| frontend.db.bodies().root_body(body));
            record.anchor = record
                .function
                .locations
                .first()
                .and_then(|location| state.metadata_anchors.owner(location));
        }
        for f in &full.functions {
            if let Some(body) = f.body_id
                && let Some((unit, owner)) = frontend.db.bodies().owner(body)
            {
                let id = match owner {
                    BodyOwner::Function(id) => Some(id),
                    BodyOwner::InterfaceDefault(id) => Some(FunctionId::method(
                        id.owner_type().unwrap(),
                        format!("$default${}", id.name()),
                    )),
                    BodyOwner::Lambda { .. } => Some(FunctionId::lambda(body)),
                    BodyOwner::Constructor { owner, .. } => Some(FunctionId::method(owner, "init")),
                    _ => None,
                };
                if let Some(id) = id {
                    state.keys.insert((unit, id), f.id.clone());
                }
                state
                    .descendants
                    .entry(frontend.db.bodies().root_body(body))
                    .or_default()
                    .insert(f.id.clone());
            }
            for callee in &f.callees {
                state
                    .reverse
                    .entry(callee.clone())
                    .or_default()
                    .insert(f.id.clone());
            }
        }
        for root in state.roots.values() {
            let source = generics::source_spelling(&root.key.1.to_string());
            if source != root.key.1.to_string() {
                let id = hash(serde_json::to_vec(&(
                    &state.units[&root.key.0].path,
                    &source,
                ))?);
                if state.records.contains_key(&id) {
                    state.keys.insert(root.key, id);
                }
            }
        }
        // Bodyless interface unions also serve as retained call targets.
        for (unit, function) in graph_keys {
            let id = hash(serde_json::to_vec(&(
                &state.units[&unit].path,
                function.to_string(),
            ))?);
            if state.records.contains_key(&id) {
                state.keys.entry((unit, function)).or_insert(id);
            }
        }
        for (body, root) in &state.roots {
            if let Ok(inputs) = indirect_inputs_tree(&frontend.db.typed_bodies, *body) {
                state.indirect.remember(root.key.0, *body, inputs);
            }
        }
        state.enforce_size()?;
        return Ok(VerificationSnapshotUpdate::Full(full));
    }
    let plan = plan.unwrap();
    let plan = plan.borrow();
    state.projection_items_cloned.set(0);
    state.boundary_functions.set(0);
    state.owner_checks = plan.owner_checks;
    state.coordinate_owners = plan.current_roots.len() + plan.declarations.len();
    ensure!(!plan.cold_fallback, VerificationSnapshotColdFallback);
    ensure!(
        state
            .dispatch
            .as_ref()
            .is_none_or(|dispatch| dispatch.supported()),
        VerificationSnapshotFallback
    );
    ensure!(!plan.fallback, VerificationSnapshotFallback);
    ensure!(
        frontend.module_graph.files.len() + 1 == state.units.len(),
        VerificationSnapshotFallback
    );
    for key in &plan.changed {
        if !state.keys.contains_key(key) {
            let path = &state.units[&key.0].path;
            state
                .keys
                .insert(*key, hash(serde_json::to_vec(&(path, key.1.to_string()))?));
        }
    }
    let retired_bodies: HashSet<_> = state
        .units
        .keys()
        .flat_map(|unit| frontend.db.effects.removed_roots(*unit))
        .collect();
    let mut removed_functions: Vec<_> = plan
        .removed
        .iter()
        .filter_map(|key| state.keys.get(key).cloned())
        .collect();
    for key in &plan.removed {
        if let Some(id) = state.keys.get(key)
            && let Some(body) = state
                .records
                .get(id)
                .and_then(|record| record.function.body_id)
            && let Some(descendants) = state.descendants.get(&body)
        {
            removed_functions.extend(descendants.iter().cloned());
        }
    }
    removed_functions.sort();
    removed_functions.dedup();
    let mut removed_ids: HashSet<_> = removed_functions.iter().cloned().collect();
    let mut affected = HashSet::new();
    let mut queue = VecDeque::new();
    let mut indirect_callers = HashSet::new();
    for body in &retired_bodies {
        if let Some(root) = state.roots.get(body) {
            indirect_callers.extend(state.indirect.replace(root.key.0, *body, None));
        }
    }

    for body in &plan.projected_bodies {
        if let Some(root) = plan.current_roots.get(body)
            && plan.current_keys.get(&root.key) != Some(body)
        {
            indirect_callers.extend(state.indirect.replace(
                root.key.0,
                *body,
                Some(indirect_inputs_tree(&frontend.db.typed_bodies, *body)?),
            ));
        }
    }
    for key in plan.changed.iter().chain(&plan.removed) {
        let body = plan.current_keys.get(key).copied().or_else(|| {
            state
                .keys
                .get(key)
                .and_then(|id| state.records.get(id))
                .and_then(|record| record.function.body_id)
        });
        if let Some(body) = body {
            let inputs = if plan.removed.contains(key) {
                None
            } else {
                Some(
                    match indirect_inputs_tree(&frontend.db.typed_bodies, body) {
                        Ok(inputs) => inputs,
                        Err(_) if frontend.db.bodies().source_body(body) != body => {
                            Default::default()
                        }
                        Err(error) => return Err(error),
                    },
                )
            };
            indirect_callers.extend(state.indirect.replace(key.0, body, inputs));
            if let Some(ids) = state.descendants.get(&body) {
                for id in ids {
                    if affected.insert(id.clone()) {
                        queue.push_back(id.clone());
                    }
                }
            }
        }
    }
    for caller in &indirect_callers {
        if let Some(id) = state.keys.get(caller)
            && affected.insert(id.clone())
        {
            queue.push_back(id.clone());
        }
    }

    for key in plan.changed.iter().chain(&plan.removed) {
        let id = state
            .keys
            .get(key)
            .ok_or(VerificationSnapshotFallback)?
            .clone();
        if affected.insert(id.clone()) {
            queue.push_back(id);
        }
    }
    for unit in state.units.keys() {
        for function in frontend.db.effects.affected_functions(*unit) {
            if let Some(id) = state.keys.get(&(*unit, function))
                && affected.insert(id.clone())
            {
                queue.push_back(id.clone());
            }
        }
    }
    while let Some(id) = queue.pop_front() {
        for caller in state.reverse.get(&id).into_iter().flatten() {
            if affected.insert(caller.clone()) {
                queue.push_back(caller.clone());
            }
        }
    }
    affected.retain(|id| !removed_ids.contains(id));
    let mut relocations = Vec::new();
    for (body, root) in &plan.current_roots {
        if let Some(old) = state.roots.get(body)
            && old.span != root.span
        {
            relocations.push(VerificationRelocation {
                path: state.units[&old.key.0].path.clone(),
                old_start: old.span.start,
                old_end: old.span.end,
                new_start: root.span.start,
                new_end: root.span.end,
            });
        }
        state.anchors.insert(
            root_anchor(*body),
            Location {
                path: state.units[&root.key.0].path.clone(),
                start: root.span.start,
                end: root.span.end,
            },
        );
        state.roots.insert(*body, root.clone());
    }
    for (key, span) in &plan.declarations {
        if let Some(old) = state.declarations.get(key)
            && old != span
        {
            relocations.push(VerificationRelocation {
                path: state.units[&key.0].path.clone(),
                old_start: old.start,
                old_end: old.end,
                new_start: span.start,
                new_end: span.end,
            });
        }
        state.anchors.insert(
            declaration_anchor(key),
            Location {
                path: state.units[&key.0].path.clone(),
                start: span.start,
                end: span.end,
            },
        );
        state.declarations.insert(key.clone(), *span);
    }
    let mut changed_ids: HashSet<_> = plan
        .changed
        .iter()
        .filter_map(|key| state.keys.get(key).cloned())
        .collect();
    for key in &plan.changed {
        if let Some(body) = plan.current_keys.get(key)
            && let Some(ids) = state.descendants.get(body)
        {
            changed_ids.extend(ids.iter().cloned());
        }
    }
    for (key, is_enum) in &plan.bodyless {
        state.bodyless.insert(key.clone(), *is_enum);
    }
    for key in plan
        .pending_declarations
        .keys()
        .filter(|key| plan.unit_items.contains_key(&key.0))
    {
        state.declarations.remove(key);
        state.anchors.remove(&declaration_anchor(key));
        if let Some((_, name)) = key.1.split_once(':') {
            state.bodyless.remove(&(key.0, name.to_owned()));
        }
    }
    let fixed_ids = RefCell::new(HashSet::new());
    let compact = |id: &str| {
        fixed_ids.borrow_mut().insert(id.to_owned());
        state.boundary_function(id)
    };
    let mut boundary = HashMap::new();
    let mut synthetic = Vec::new();
    for id in &affected {
        let Some(record) = state.records.get(id) else {
            continue;
        };
        let Some(body) = record.function.body_id else {
            let mut function = state.current_function(id);
            function.unknown = !function.unresolved.is_empty();
            function.runtime_effects = if function.unknown {
                RuntimeEffects::ALL.bits()
            } else {
                0
            };
            synthetic.push(function);
            continue;
        };
        let Some((unit, function)) = callable_owner(frontend.db.bodies(), body) else {
            continue;
        };
        if !changed_ids.contains(id) {
            let mut f = state.current_function(id);
            f.runtime_effects = state.own.get(&(unit, function)).copied().unwrap_or(0)
                | frontend
                    .db
                    .effects
                    .effect_capabilities(unit, function)
                    .map_or(0, |c| c.runtime.intersection(RuntimeEffects::ALL).bits());
            f.unknown = !f.unresolved.is_empty();
            if f.unknown {
                f.runtime_effects |= RuntimeEffects::ALL.bits();
            }
            boundary.insert((unit, function), f);
        }
        for callee in &record.function.callees {
            if affected.contains(callee) {
                continue;
            }
            if let Some(record) = state.records.get(callee)
                && let Some(body) = record.function.body_id
                && let Some((unit, function)) = callable_owner(frontend.db.bodies(), body)
            {
                boundary
                    .entry((unit, function))
                    .or_insert_with(|| compact(callee));
            }
        }
    }
    // Newly introduced direct targets may lie outside the old dependency closure.
    if let Some(captures) = frontend.db.effects.analysis.borrow().as_ref() {
        for (&unit, capture) in captures {
            for (_, sites) in capture.graph.iter() {
                for target in &sites.targets {
                    if let Some(id) = state.keys.get(&(unit, *target))
                        && !affected.contains(id)
                    {
                        boundary
                            .entry((unit, *target))
                            .or_insert_with(|| compact(id));
                    }
                }
            }
        }
    }
    let affected_synthetic: HashSet<_> = synthetic
        .iter()
        .map(|function| function.id.clone())
        .collect();
    let lookup = |unit, function| {
        state.keys.get(&(unit, function)).map(|id| {
            if affected_synthetic.contains(id) {
                let mut function = state.current_function(id);
                function.unknown = !function.unresolved.is_empty();
                function.runtime_effects = if function.unknown {
                    RuntimeEffects::ALL.bits()
                } else {
                    0
                };
                function
            } else {
                compact(id)
            }
        })
    };
    let bodyless = |unit, function: FunctionId| {
        let is_enum = *state.bodyless.get(&(unit, function.owner()?.to_string()))?;
        if is_enum {
            Some(RuntimeEffects::MAY_ALLOCATE)
        } else if function.name().as_ref() == "init" {
            Some(RuntimeEffects::NONE)
        } else {
            None
        }
    };
    for id in &affected {
        if let Some(body) = state
            .records
            .get(id)
            .and_then(|record| record.function.body_id)
            && let Some(caller) = callable_owner(frontend.db.bodies(), body)
        {
            indirect_callers.insert(caller);
        }
    }
    let indirect_inputs = state.indirect.inputs(&indirect_callers);
    let mut generic_names: HashMap<UnitId, HashSet<String>> = HashMap::new();
    for (unit, id) in &plan.changed {
        generic_names
            .entry(*unit)
            .or_default()
            .insert(generics::source_spelling(&id.to_string()));
        if let Some(owner) = id.owner() {
            generic_names
                .entry(*unit)
                .or_default()
                .insert(owner.to_string());
        }
    }
    for id in &affected {
        if let Some(record) = state.records.get(id)
            && let Some((unit, _)) = record.key
        {
            generic_names
                .entry(unit)
                .or_default()
                .insert(generics::source_spelling(&record.function.name));
        }
    }
    for body in &retired_bodies {
        if let Some(root) = state.roots.get(body) {
            generic_names
                .entry(root.key.0)
                .or_default()
                .insert(generics::source_spelling(&root.key.1.to_string()));
        }
    }
    let mut selected_generic_sources = HashMap::new();
    for (unit, names) in generic_names {
        if let Some(source) = frontend.db.generic_sources.get(&unit) {
            let selected = source
                .selected_functions(&names)
                .ok_or(VerificationSnapshotFallback)?;
            if selected.has_workspace_generics() {
                for name in selected.items.keys() {
                    let id = hash(serde_json::to_vec(&(&state.units[&unit].path, name))?);
                    changed_ids.insert(id.clone());
                    affected.insert(id);
                }
                selected_generic_sources.insert(unit, selected);
            }
        }
    }
    let dispatch = RefCell::new(state.dispatch.clone());
    let dispatch_affected = RefCell::new(HashSet::new());
    let dispatch_visits = std::cell::Cell::new(0);
    let dispatch_removed = RefCell::new(HashSet::new());
    let retired_dispatch: HashSet<_> = changed_ids.iter().chain(&removed_ids).cloned().collect();
    let lookup_id = |id: &str| {
        state
            .records
            .contains_key(id)
            .then(|| state.current_function(id))
    };
    let compact_id = |id: &str| state.records.contains_key(id).then(|| compact(id));
    let dispatch_dependents = |changed: &[Function], removed: &[String], visits: usize| {
        dispatch_visits.set(visits);
        let mut ids: HashSet<_> = changed
            .iter()
            .map(|function| function.id.clone())
            .chain(removed.iter().cloned())
            .collect();
        let mut queue: VecDeque<_> = ids.iter().cloned().collect();
        while let Some(id) = queue.pop_front() {
            for caller in state.reverse.get(&id).into_iter().flatten() {
                if ids.insert(caller.clone()) {
                    queue.push_back(caller.clone());
                }
            }
        }
        let mut result = Vec::new();
        let removed_set: HashSet<_> = removed.iter().collect();
        for id in &ids {
            if removed_set.contains(id) {
                continue;
            }
            if let Some(mut function) = lookup_id(id) {
                fixed_ids.borrow_mut().remove(id);
                function.unknown = !function.unresolved.is_empty();
                function.runtime_effects = if function.unknown {
                    RuntimeEffects::ALL.bits()
                } else {
                    function
                        .body_id
                        .and_then(|body| callable_owner(frontend.db.bodies(), body))
                        .map_or(0, |key| {
                            state.own.get(&key).copied().unwrap_or(0)
                                | frontend
                                    .db
                                    .effects
                                    .effect_capabilities(key.0, key.1)
                                    .map_or(0, |caps| {
                                        caps.runtime.intersection(RuntimeEffects::ALL).bits()
                                    })
                        })
                };
                result.push(function);
            }
        }
        let dependencies: HashSet<_> = changed
            .iter()
            .chain(result.iter())
            .flat_map(|function| function.callees.iter())
            .filter(|id| !ids.contains(*id))
            .cloned()
            .collect();
        result.extend(dependencies.iter().filter_map(|id| compact_id(id)));
        dispatch_affected.borrow_mut().extend(ids);
        dispatch_removed
            .borrow_mut()
            .extend(removed.iter().cloned());
        result
    };
    let instances = RefCell::new(Vec::new());
    let mut patch = snapshot_with_boundary(
        frontend,
        entry,
        source,
        project,
        SnapshotBoundary {
            instances: Some(&instances),
            functions: boundary,
            synthetic,
            dispatch: Some(&dispatch),
            dispatch_retired: Some(&retired_dispatch),
            dispatch_dependents: Some(&dispatch_dependents),
            retained_id: Some(&compact_id),
            fixed_ids: Some(&fixed_ids),
            retained: Some(&lookup),
            bodyless: Some(&bodyless),
            indirect: Some(&indirect_inputs),
            generics: (!selected_generic_sources.is_empty()).then_some(&selected_generic_sources),
        },
    )?;
    state.dispatch = dispatch.into_inner();
    state.dispatch_visits = dispatch_visits.get();
    affected.extend(dispatch_affected.into_inner());
    removed_ids.extend(dispatch_removed.into_inner());
    let mut current_changed = HashSet::new();
    for function in &patch.functions {
        if let Some(body) = function.body_id {
            let root = frontend.db.bodies().root_body(body);
            if callable_owner(frontend.db.bodies(), root)
                .is_some_and(|key| plan.changed.contains(&key))
            {
                affected.insert(function.id.clone());
                changed_ids.insert(function.id.clone());
                current_changed.insert(function.id.clone());
                if let Some(key) = callable_owner(frontend.db.bodies(), body) {
                    state.keys.insert(key, function.id.clone());
                }
            }
        }
        if function.id.starts_with("$indirect-arity:") {
            affected.insert(function.id.clone());
        }
    }
    for key in &plan.changed {
        if let Some(body) = plan.current_keys.get(key)
            && let Some(old) = state.descendants.get(body)
        {
            for id in old {
                if !current_changed.contains(id) {
                    removed_ids.insert(id.clone());
                }
            }
        }
    }
    let mut bucket_queue: Vec<_> = changed_ids
        .iter()
        .chain(&removed_ids)
        .filter_map(|id| state.records.get(id))
        .flat_map(|record| {
            record
                .function
                .callees
                .iter()
                .filter(|id| id.starts_with("$indirect-arity:"))
                .cloned()
        })
        .collect();
    let mut seen_buckets = HashSet::new();
    while let Some(id) = bucket_queue.pop() {
        if !seen_buckets.insert(id.clone()) {
            continue;
        }
        if !state.indirect.active(&id) {
            removed_ids.insert(id.clone());
        }
        if let Some(record) = state.records.get(&id) {
            bucket_queue.extend(
                record
                    .function
                    .callees
                    .iter()
                    .filter(|id| id.starts_with("$indirect-arity:"))
                    .cloned(),
            );
        }
    }
    state.replace_instances(instances.into_inner(), &removed_ids);
    removed_functions = removed_ids.iter().cloned().collect();
    removed_functions.sort();
    patch
        .functions
        .retain(|f| affected.contains(&f.id) && !removed_ids.contains(&f.id));
    let mut changed_ranges: HashMap<&str, Vec<(usize, usize)>> = HashMap::new();
    for f in patch
        .functions
        .iter()
        .filter(|f| changed_ids.contains(&f.id))
    {
        for l in &f.locations {
            changed_ranges
                .entry(&l.path)
                .or_default()
                .push((l.start, l.end));
        }
    }
    for ranges in changed_ranges.values_mut() {
        ranges.sort_unstable();
    }
    patch.semantic.symbols.retain(|s| {
        if s.id.starts_with("builtin:") {
            return true;
        }
        s.location.as_ref().is_some_and(|l| {
            let Some(ranges) = changed_ranges.get(l.path.as_str()) else {
                return false;
            };
            let at = ranges.partition_point(|(start, _)| *start <= l.start);
            at.checked_sub(1)
                .and_then(|i| ranges.get(i))
                .is_some_and(|(_, end)| l.end <= *end)
        })
    });
    let changed_types: HashMap<_, _> = patch
        .semantic
        .symbols
        .iter()
        .map(|symbol| (symbol.id.as_str(), &symbol.ty))
        .collect();
    for reference in &mut patch.semantic.references {
        if reference.ty.is_some()
            && changed_types
                .get(reference.target.as_str())
                .copied()
                .or_else(|| state.symbol_types.get(&reference.target).map(AsRef::as_ref))
                == Some(&reference.ty)
        {
            reference.ty = None;
        }
    }
    let present_symbols: HashSet<_> = patch
        .semantic
        .symbols
        .iter()
        .map(|s| s.id.as_str())
        .collect();
    let removed_symbols: Vec<_> = changed_ids
        .iter()
        .chain(&removed_ids)
        .filter_map(|id| state.records.get(id))
        .flat_map(|record| record.semantic.symbols.iter().map(|s| s.id.clone()))
        .filter(|id| !present_symbols.contains(id.as_str()))
        .collect();
    // Effect-only projections reuse their body-local semantic data; only the
    // solved effect value and source coordinates are refreshed.
    for function in &patch.functions {
        if changed_ids.contains(&function.id) || function.body_id.is_none() {
            continue;
        }
        let mut cached = state.current_record(&function.id, true);
        let body = function.body_id.unwrap();
        let (unit, id) = callable_owner(frontend.db.bodies(), body).expect("callable body");
        if let Some(e) = frontend.db.effects.effect_capabilities(unit, id) {
            cached.semantic.compiler_effects.insert(
                function.id.clone(),
                e.runtime.intersection(RuntimeEffects::ALL).bits(),
            );
        }
        merge_semantic(&mut patch.semantic, cached.semantic);
    }
    let body_locations: HashMap<_, _> = patch
        .functions
        .iter()
        .filter_map(|function| Some((function.body_id?, function.locations.first()?.start)))
        .collect();
    let (_, records) = split(&patch);
    for id in &removed_functions {
        state.dirty_instance_owners.insert(id.clone());
        if let Some(old) = state.records.remove(id) {
            for callee in &old.function.callees {
                if let Some(callers) = state.reverse.get_mut(callee) {
                    callers.remove(id);
                }
            }
            if let Some(body) = old.function.body_id {
                if let Some(key) = old.key
                    && state.keys.get(&key).is_some_and(|current| current == id)
                {
                    state.keys.remove(&key);
                    state.own.remove(&key);
                }
                let root = old.root.unwrap_or(body);
                if let Some(ids) = state.descendants.get_mut(&root) {
                    ids.remove(id);
                }
                if state
                    .descendants
                    .get(&root)
                    .is_some_and(|ids| ids.is_empty())
                {
                    state.descendants.remove(&root);
                }
                state.roots.remove(&body);
                state.anchors.remove(&root_anchor(body));
            }
        }
        state.reverse.remove(id);
        state.bytes = state
            .bytes
            .saturating_sub(state.encoded.remove(id).unwrap_or(0));
    }
    for key in &plan.removed {
        state.keys.remove(key);
        state.own.remove(key);
    }
    for (id, mut record) in records {
        record.order_path = order_path(
            frontend.db.bodies(),
            record.function.body_id,
            &body_locations,
        );
        record.key = record
            .function
            .body_id
            .and_then(|body| callable_owner(frontend.db.bodies(), body));
        record.root = record
            .function
            .body_id
            .map(|body| frontend.db.bodies().root_body(body));
        record.anchor = state
            .records
            .get(&id)
            .and_then(|old| old.anchor.as_ref())
            .and_then(|(key, _)| {
                state
                    .anchors
                    .get(key)
                    .map(|location| (key.clone(), location.clone()))
            })
            .or_else(|| {
                record.function.body_id.and_then(|body| {
                    let key = root_anchor(frontend.db.bodies().root_body(body));
                    state
                        .anchors
                        .get(&key)
                        .map(|location| (key.clone(), location.clone()))
                })
            });
        if let Some(old) = state.records.get(&id) {
            record.order = old.order;
        }
        if let Some(old) = state.records.get(&id) {
            for callee in &old.function.callees {
                if let Some(callers) = state.reverse.get_mut(callee) {
                    callers.remove(&id);
                }
            }
        }
        for callee in &record.function.callees {
            state
                .reverse
                .entry(callee.clone())
                .or_default()
                .insert(id.clone());
        }
        let size = encoded(&record)?;
        state.bytes = state
            .bytes
            .saturating_sub(state.encoded.get(&id).copied().unwrap_or(0))
            + size;
        state.encoded.insert(id.clone(), size);
        if let Some(body) = record.function.body_id {
            state
                .descendants
                .entry(frontend.db.bodies().root_body(body))
                .or_default()
                .insert(id.clone());
        }
        state.dirty_instance_owners.insert(id.clone());
        state.records.insert(id, Arc::new(record));
    }
    for id in &removed_symbols {
        state.symbol_types.remove(id);
    }
    for symbol in &patch.semantic.symbols {
        if !symbol.id.starts_with("builtin:") {
            state
                .symbol_types
                .insert(symbol.id.clone(), Arc::new(symbol.ty.clone()));
        }
    }
    for body in retired_bodies {
        if let Some(root) = state.roots.remove(&body)
            && !plan.current_keys.contains_key(&root.key)
        {
            state.keys.remove(&root.key);
            state.own.remove(&root.key);
        }
        state.anchors.remove(&root_anchor(body));
        state.descendants.remove(&body);
    }
    for (key, bits) in own {
        state.own.insert(key, bits);
    }
    state.header = Some(Header::from(&patch));
    for (unit, count) in &plan.unit_items {
        if let Some(value) = state.units.get_mut(unit) {
            value.items = *count;
        }
    }
    state.enforce_size()?;
    Ok(VerificationSnapshotUpdate::Delta {
        patch,
        removed_functions,
        removed_symbols,
        relocations,
    })
}
