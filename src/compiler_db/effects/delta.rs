use super::*;

// A retained unit consumes changed typed owners directly. Structural declaration
// invalidation still goes through the full producer so synthetic declarations and
// copied defaults are never inferred from a stale source inventory.
pub(super) struct DeltaOrigin<'a> {
    pub(super) index: &'a crate::compiler_db::ids::BodyIndex,
    pub(super) queries: &'a EffectQueries,
    pub(super) unit: UnitId,
}

pub(super) fn solve_unit_delta<N: std::fmt::Display>(
    program: &Program,
    graph: &CallGraph,
    types: &HashMap<ExprId, Type<N>>,
    origin: DeltaOrigin<'_>,
    callables: &HashMap<FunctionId, bool>,
    direct: &HashMap<FunctionId, DirectLockEffects>,
    external: &impl Fn(&FunctionId) -> RuntimeEffects,
) -> Option<UnitEffects> {
    let DeltaOrigin {
        index,
        queries,
        unit,
    } = origin;
    let tracking = queries.tracking.borrow().upgrade()?;
    let source_changes = tracking.borrow().effect_source_changes(unit)?;
    // Nominal declaration/dispatch changes rebuild the unit producer inventory.
    // Individual free callable additions, removals and signatures have complete
    // owner deltas and do not take that path.
    if source_changes
        .iter()
        .any(|(owner, _)| owner.function_name().is_none())
    {
        return None;
    }
    let previous = queries.previous.borrow().get(&unit)?.clone();
    let locations = queries.root_locations.borrow();
    let delta = DeltaSolve {
        program,
        graph,
        index,
        queries,
        unit,
        callables,
        direct,
        locations: &locations,
        tracking: &tracking,
    };
    let DirtyRoots {
        inherited_changed,
        mut changed_copies,
        removed_roots,
        created_copies,
        roots,
    } = delta.dirty_roots(&previous, &source_changes)?;
    if previous.plan.ambiguous {
        return None;
    }
    let mut result = (*previous).clone();
    delta.register_created_copies(&mut result, created_copies);
    let mut solution = (**queries.solutions.borrow().get(&unit)?).clone();
    let mut io_solution = (**queries.io_solutions.borrow().get(&unit)?).clone();
    let mut changed = delta.rescan_roots(&mut result, roots, &removed_roots, types)?;
    for (owner, exists) in source_changes {
        if !exists {
            result.plan.source_roots.remove(owner.wire_key());
        }
    }
    let imports = import_aliases(program);
    let old_external_targets: HashSet<_> = changed
        .iter()
        .filter_map(|&id| solution.equation(id))
        .flat_map(|equation| equation.edges.iter().map(|(target, _)| *target))
        .filter(|target| !previous.preemption_bodies.contains(target))
        .collect();
    changed.extend(inherited_changed);
    let mut seeds = DeltaSeeds {
        external: delta.propagate_dirty_sources(&result, &changed, &mut changed_copies),
        ..DeltaSeeds::default()
    };
    delta.seed_changed_bodies(&mut result, &changed, &mut changed_copies, &mut seeds);
    delta.seed_changed_copies(&mut result, &changed_copies, &mut seeds);
    changed.extend(changed_copies.iter().copied());
    delta.seed_external_targets(&mut result, &mut seeds, &imports, external);
    changed.extend(seeds.external.iter().copied());
    delta.seed_helper_preemption(&result, &solution, &changed, &mut seeds);
    let DeltaSeeds {
        seeds,
        io: io_seeds,
        removed,
        ..
    } = seeds;
    let mut compared = delta.update_lock_equations(&mut result, &mut solution, seeds, &removed);
    compared += delta.update_io_equations(&result, &mut io_solution, &changed, &io_seeds, &removed);
    delta.prune_external_leaves(
        &mut result,
        &mut solution,
        &mut io_solution,
        old_external_targets,
        &imports,
    );
    Some(delta.publish(result, solution, io_solution, &changed, compared))
}

/// Inputs shared by every stage of a retained-unit delta.
pub(super) struct DeltaSolve<'a> {
    pub(super) program: &'a Program,
    pub(super) graph: &'a CallGraph,
    pub(super) index: &'a crate::compiler_db::ids::BodyIndex,
    pub(super) queries: &'a EffectQueries,
    pub(super) unit: UnitId,
    pub(super) callables: &'a HashMap<FunctionId, bool>,
    pub(super) direct: &'a HashMap<FunctionId, DirectLockEffects>,
    pub(super) locations: &'a HashMap<(UnitId, BodyId), EffectRootLocation>,
    pub(super) tracking: &'a std::cell::RefCell<crate::compiler_db::incremental::SyntaxQueries>,
}

/// Declaration roots and copies a delta must revisit.
pub(super) struct DirtyRoots {
    /// Bodies inheriting facts from a changed provider callable.
    pub(super) inherited_changed: HashSet<FunctionId>,
    /// Canonical copies whose source or destination changed.
    pub(super) changed_copies: HashSet<FunctionId>,
    pub(super) removed_roots: HashSet<BodyId>,
    pub(super) created_copies: Vec<(FunctionId, CopyContribution)>,
    /// Changed declaration roots to rescan.
    pub(super) roots: HashSet<BodyId>,
}

/// Replacement equations collected before they are applied.
#[derive(Default)]
pub(super) struct DeltaSeeds {
    pub(super) seeds: HashMap<FunctionId, EffectSummary<EffectWitness>>,
    pub(super) io: HashSet<FunctionId>,
    pub(super) removed: HashSet<FunctionId>,
    /// Callees outside this unit's bodies whose external equation is refreshed.
    pub(super) external: HashSet<FunctionId>,
}

/// Seeds what a provider's retained summary contributes to an inheriting body.
pub(super) fn inherit_summary(
    seed: &mut EffectSummary<EffectWitness>,
    summary: &EffectSummary<EffectWitness>,
) {
    seed.add_effect(summary.effects().intersection(ANY_PANIC), None);
    seed.add_effect(
        summary.effects().intersection(LOCK_EFFECT_WAIT),
        summary.witness(LOCK_EFFECT_WAIT),
    );
}

impl DeltaSolve<'_> {
    pub(super) fn dirty_roots(
        &self,
        previous: &UnitEffects,
        source_changes: &[(SourceOwner, bool)],
    ) -> Option<DirtyRoots> {
        let (index, unit, locations) = (self.index, self.unit, self.locations);
        let mut inherited_changed = HashSet::new();
        for &provider in &previous.plan.providers {
            if let Some(sources) = self.queries.affected.borrow().get(&provider) {
                for source in sources {
                    if let Some(targets) = previous.plan.inherited_targets.get(&(provider, *source))
                    {
                        inherited_changed.extend(targets.iter().copied());
                    }
                }
            }
        }
        let mut changed_copies = HashSet::new();
        for provider in previous.plan.copy_sources.keys() {
            if let Some(sources) = self.queries.affected.borrow().get(provider) {
                for source in sources {
                    if let Some(targets) = previous.plan.copy_targets.get(&(*provider, *source)) {
                        changed_copies.extend(targets.iter().copied());
                    }
                }
            }
        }
        let mut removed_roots: HashSet<_> = source_changes
            .iter()
            .filter(|(_, exists)| !exists)
            .flat_map(|(owner, _)| {
                previous
                    .plan
                    .source_roots
                    .get(owner.wire_key())
                    .into_iter()
                    .flatten()
                    .copied()
            })
            .collect();
        removed_roots.extend(self.queries.removed_roots(unit));
        let mut created_copies = Vec::new();
        let mut roots = HashSet::new();
        for mut body in self.tracking.borrow().changed_bodies_in(unit) {
            let Some((owner_unit, mut owner)) = index.owner(body) else {
                continue;
            };
            if owner_unit != unit {
                continue;
            }
            while let crate::compiler_db::ids::BodyOwner::Lambda { parent, .. } = owner {
                body = parent;
                owner = index.owner(body)?.1;
            }
            let source = index.source_body(body);
            if source != body
                && previous
                    .plan
                    .copies
                    .contains_key(&source_callable(index, body))
            {
                changed_copies.insert(source_callable(index, body));
                continue;
            }
            let reused_default = match locations.get(&(unit, body)) {
                Some(EffectRootLocation::Method { item, member }) => {
                    match self.program.items.get(*item) {
                        Some(Item::Class(class)) => class
                            .methods
                            .get(*member)
                            .is_some_and(|method| method.is_default_injected),
                        _ => false,
                    }
                }
                _ => false,
            };
            if source != body && (locations.contains_key(&(unit, source)) || reused_default) {
                let (provider, _) = index.owner(source)?;
                let id = source_callable(index, body);
                created_copies.push((
                    id,
                    CopyContribution {
                        body,
                        source_body: source,
                        provider,
                        source: source_callable(index, source),
                    },
                ));
                changed_copies.insert(id);
                continue;
            }
            roots.insert(body);
        }
        // A signature-only owner edit (for example toggling `async`) keeps the body
        // text but changes its callable kind and helper membership; rescan it.
        for (owner, _) in source_changes.iter().filter(|(_, exists)| *exists) {
            roots.extend(
                previous
                    .plan
                    .source_roots
                    .get(owner.wire_key())
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|body| {
                        !removed_roots.contains(body)
                            && locations.contains_key(&(unit, *body))
                            && index.owner(*body).is_some_and(|(owner, _)| owner == unit)
                    }),
            );
        }
        // All root locations come from the existing declaration inventory pass.
        for &root in &roots {
            locations.get(&(unit, root))?.block(self.program)?;
        }
        Some(DirtyRoots {
            inherited_changed,
            changed_copies,
            removed_roots,
            created_copies,
            roots,
        })
    }

    pub(super) fn register_created_copies(
        &self,
        result: &mut UnitEffects,
        created_copies: Vec<(FunctionId, CopyContribution)>,
    ) {
        for (id, copy) in created_copies {
            result.plan.copies.insert(id, copy);
            result
                .plan
                .copy_targets
                .entry((copy.provider, copy.source))
                .or_default()
                .insert(id);
            result
                .plan
                .copy_sources
                .entry(copy.provider)
                .or_default()
                .insert(copy.source);
            result.plan.bodies.insert(id, copy.body);
            result.plan.roots.insert(copy.body, Arc::new(vec![id]));
            result.preemption_bodies.insert(id);
            result
                .preemption_copies
                .insert((id, copy.provider, copy.source));
            if let Some(&is_async) = self.callables.get(&id) {
                result.callables.insert(id, is_async);
                if !is_async
                    && let Some(span) = self
                        .locations
                        .get(&(self.unit, copy.body))
                        .and_then(|location| location.span(self.program))
                {
                    result.plan.helpers.insert(id, span);
                }
            }
        }
    }

    /// Drops the retained parts of changed and removed roots, then rescans each
    /// changed root. Returns every callable whose direct parts changed.
    pub(super) fn rescan_roots<N: std::fmt::Display>(
        &self,
        result: &mut UnitEffects,
        roots: HashSet<BodyId>,
        removed_roots: &HashSet<BodyId>,
        types: &HashMap<ExprId, Type<N>>,
    ) -> Option<HashSet<FunctionId>> {
        let mut changed = HashSet::new();
        for root in roots.into_iter().chain(removed_roots.iter().copied()) {
            for &id in result
                .plan
                .roots
                .get(&root)
                .map(Arc::as_ref)
                .into_iter()
                .flatten()
            {
                changed.insert(id);
                if let Some(parts) = result.plan.direct.get_mut(&id) {
                    if let Some(old) = parts.remove(&root) {
                        result.loop_bodies.remove(&old.body);
                    }
                    if parts.is_empty() {
                        result.plan.direct.remove(&id);
                    }
                }
            }
            if removed_roots.contains(&root) {
                forget_removed_root(result, root);
                continue;
            }
            self.rescan_root(result, root, types, &mut changed)?;
        }
        Some(changed)
    }

    pub(super) fn rescan_root<N: std::fmt::Display>(
        &self,
        result: &mut UnitEffects,
        root: BodyId,
        types: &HashMap<ExprId, Type<N>>,
        changed: &mut HashSet<FunctionId>,
    ) -> Option<()> {
        let (program, index, queries) = (self.program, self.index, self.queries);
        let location = self.locations[&(self.unit, root)];
        let body = location.block(program)?;
        let id = source_callable(index, root);
        let source = index.source_body(root);
        if source != root {
            let (provider, _) = index.owner(source)?;
            let source_id = source_callable(index, source);
            result.plan.inherited.insert(
                root,
                CopyContribution {
                    body: root,
                    source_body: source,
                    provider,
                    source: source_id,
                },
            );
            result
                .plan
                .inherited_targets
                .entry((provider, source_id))
                .or_default()
                .insert(id);
        }

        if let Some(&is_async) = self.callables.get(&id) {
            result.callables.insert(id, is_async);
            if is_async {
                result.plan.helpers.remove(&id);
            } else if matches!(
                location,
                EffectRootLocation::Function(_) | EffectRootLocation::Method { .. }
            ) {
                result.plan.helpers.insert(id, location.span(program)?);
            }
        }
        if let Some(owner) = self.tracking.borrow().body_source_owner(root) {
            result
                .plan
                .source_roots
                .entry(owner)
                .or_default()
                .insert(root);
        }
        let scan = || {
            queries.scanned.set(queries.scanned.get() + 1);
            let contract = match location {
                EffectRootLocation::Function(item) => match program.items.get(item) {
                    Some(Item::Function(f)) => contract_runtime_effects(f),
                    _ => None,
                },
                _ => None,
            };
            Ok(with_contract_effects(
                scan_direct_effects(id, root, body, types, Some(index), queries.overflow_checks),
                contract,
            ))
        };
        let scan = match queries.body_queries.borrow().upgrade() {
            Some(bodies) => bodies
                .derived(
                    root,
                    crate::compiler_db::tracked::QueryNode::DirectEffects(root),
                    None,
                    scan,
                )
                .expect("changed direct effect dependencies have completed"),
            None => scan().expect("direct effect scan is infallible"),
        };
        result.plan.roots.insert(
            root,
            Arc::new(
                scan.iter()
                    .map(|d| d.id)
                    .chain(graph_lambdas(root, body, Some(index)))
                    .collect(),
            ),
        );
        changed.extend(result.plan.roots[&root].iter().copied());
        if result.plan.helpers.contains_key(&id) {
            result.plan.helpers.insert(id, location.span(program)?);
        }
        for part in scan {
            changed.insert(part.id);
            result.plan.bodies.insert(part.id, part.body);
            result
                .plan
                .direct
                .entry(part.id)
                .or_default()
                .insert(root, Arc::new(part));
        }
        Some(())
    }

    /// Closes changed copies over same-unit copy sources, then collects the
    /// external callees of changed providers. Returns those external callees.
    pub(super) fn propagate_dirty_sources(
        &self,
        result: &UnitEffects,
        changed: &HashSet<FunctionId>,
        changed_copies: &mut HashSet<FunctionId>,
    ) -> HashSet<FunctionId> {
        let mut copy_pending: Vec<_> = changed.iter().copied().collect();
        while let Some(source) = copy_pending.pop() {
            if let Some(targets) = result.plan.copy_targets.get(&(self.unit, source)) {
                for &id in targets {
                    if changed_copies.insert(id) {
                        copy_pending.push(id);
                    }
                }
            }
        }
        let mut external_changed = HashSet::new();
        let affected_units = self.queries.affected.borrow();
        for &provider in &result.plan.providers {
            let Some(affected) = affected_units.get(&provider) else {
                continue;
            };
            for &function in affected {
                if let Some(targets) = result.plan.external.get(&(provider, function)) {
                    external_changed.extend(targets.iter().copied());
                }
            }
        }
        external_changed
    }

    /// Seeds unknown calls and collects external callees of `id`. Returns
    /// whether `id` has a graph vertex.
    pub(super) fn seed_call_sites(
        &self,
        preemption_bodies: &im::HashSet<FunctionId>,
        id: FunctionId,
        seed: &mut EffectSummary<EffectWitness>,
        seeds: &mut DeltaSeeds,
    ) -> bool {
        let Some(sites) = self.graph.get(&id) else {
            return false;
        };
        if sites.has_unknown {
            seed.add_effect(PANIC, None);
            seeds.io.insert(id);
        }
        seeds.external.extend(
            sites
                .targets
                .iter()
                .filter(|target| !preemption_bodies.contains(target))
                .copied(),
        );
        true
    }

    pub(super) fn seed_direct_locks(
        &self,
        id: FunctionId,
        seed: &mut EffectSummary<EffectWitness>,
    ) {
        for cause in self.direct.get(&id).into_iter().flat_map(|d| &d.causes) {
            seed.add_effect(cause.kind.effects(), lock_cause_witness(id, cause).as_ref());
        }
    }

    pub(super) fn seed_changed_bodies(
        &self,
        result: &mut UnitEffects,
        changed: &HashSet<FunctionId>,
        changed_copies: &mut HashSet<FunctionId>,
        seeds: &mut DeltaSeeds,
    ) {
        for &id in changed {
            if result.plan.copies.contains_key(&id) {
                changed_copies.insert(id);
                continue;
            }
            result.plan.loops.remove(&id);
            result.preemption_loops.remove(&id);
            if !result.plan.direct.contains_key(&id) {
                result.preemption_bodies.remove(&id);
                // A block lambda can become an expression lambda while retaining
                // its contextual identity. Its graph vertex still exists even
                // though it no longer has a scanned block contribution.
                let mut seed = EffectSummary::default();
                if self.seed_call_sites(&result.preemption_bodies, id, &mut seed, seeds) {
                    seeds.seeds.insert(id, seed);
                } else {
                    seeds.removed.insert(id);
                }
                continue;
            }
            result.preemption_bodies.insert(id);
            let mut seed = self.seed_scanned_parts(result, id, seeds);
            self.seed_direct_locks(id, &mut seed);
            if !self.seed_call_sites(&result.preemption_bodies, id, &mut seed, seeds) {
                seed.add_effect(PANIC, None);
            }
            seeds.seeds.insert(id, seed);
        }
    }

    /// Seeds the retained scanned parts of `id` in the cold producer's order.
    pub(super) fn seed_scanned_parts(
        &self,
        result: &mut UnitEffects,
        id: FunctionId,
        seeds: &mut DeltaSeeds,
    ) -> EffectSummary<EffectWitness> {
        let (unit, locations) = (self.unit, self.locations);
        let mut seed = EffectSummary::default();
        let parts = &result.plan.direct[&id];
        // The cold producer pops declaration roots in reverse source order.
        // Preserve that deterministic witness preference for overloaded owners.
        let mut ordered_parts: Vec<_> = parts.iter().collect();
        ordered_parts.sort_by_key(|(root, _)| {
            std::cmp::Reverse(match locations.get(&(unit, **root)) {
                Some(EffectRootLocation::Function(item)) => (*item, 0),
                Some(
                    EffectRootLocation::Method { item, member }
                    | EffectRootLocation::Constructor { item, member }
                    | EffectRootLocation::InterfaceDefault { item, member },
                ) => (*item, *member),
                None => (0, 0),
            })
        });
        for (&root, part) in ordered_parts {
            if part.body == root
                && let Some(copy) = result.plan.inherited.get(&root)
            {
                let known =
                    self.queries
                        .source_effects(unit, copy.provider, copy.source_body, copy.source);
                if let Some((summary, has_loop, may_io)) = known {
                    inherit_summary(&mut seed, &summary);
                    if may_io {
                        seeds.io.insert(id);
                    }
                    if has_loop {
                        result.loop_bodies.insert(root);
                        if let Some(&span) = result.plan.helpers.get(&id) {
                            result.plan.loops.insert(id, span);
                        }
                    } else if part.loop_span.is_none() {
                        result.loop_bodies.remove(&root);
                    }
                } else {
                    seed.add_effect(PANIC, None);
                }
            }
            if part.preemption_loop {
                result.preemption_loops.insert(id);
            }
            if let Some(span) = part.loop_span {
                result.loop_bodies.insert(part.body);
                result.plan.loops.insert(id, span);
            }
            if part.may_io {
                seeds.io.insert(id);
            }
            if let Some(span) = part.lock_span {
                seed.add_effect(
                    RuntimeEffects::MAY_SUSPEND,
                    Some(&lock_acquisition_witness(
                        id,
                        span,
                        part.lock_target.as_ref(),
                    )),
                );
            }
            if let Some((bits, witness)) = direct_panic(id, part) {
                seed.add_effect(bits, witness.as_ref());
            }
        }
        seed
    }

    /// Canonical copies have no destination body scan. Refresh only aliases
    /// reached from changed source owners, preserving the full producer's masks.
    pub(super) fn seed_changed_copies(
        &self,
        result: &mut UnitEffects,
        changed_copies: &HashSet<FunctionId>,
        seeds: &mut DeltaSeeds,
    ) {
        let (unit, locations) = (self.unit, self.locations);
        for &id in changed_copies {
            let copy = result.plan.copies[&id];
            if result.plan.helpers.contains_key(&id)
                && let Some(span) = locations
                    .get(&(unit, copy.body))
                    .and_then(|location| location.span(self.program))
            {
                result.plan.helpers.insert(id, span);
            }
            result.plan.loops.remove(&id);
            result.loop_bodies.remove(&copy.body);
            let mut seed = EffectSummary::default();
            if copy.provider == unit {
                if let Some(&span) = result.plan.loops.get(&copy.source) {
                    result.plan.loops.insert(id, span);
                }
            } else {
                let known =
                    self.queries
                        .source_effects(unit, copy.provider, copy.source_body, copy.source);
                if let Some((summary, has_loop, may_io)) = known {
                    inherit_summary(&mut seed, &summary);
                    if may_io {
                        seeds.io.insert(id);
                    }
                    if has_loop {
                        result.loop_bodies.insert(copy.body);
                        if let Some(&span) = result.plan.helpers.get(&id) {
                            result.plan.loops.insert(id, span);
                        }
                    }
                } else {
                    seed.add_effect(PANIC.union(LOCK_EFFECT_WAIT), None);
                    seeds.io.insert(id);
                    if let Some(&span) = result.plan.helpers.get(&id) {
                        result.plan.loops.insert(id, span);
                    }
                }
            }
            self.seed_direct_locks(id, &mut seed);
            if !self.seed_call_sites(&result.preemption_bodies, id, &mut seed, seeds)
                && copy.provider != unit
            {
                seed.add_effect(PANIC, None);
            }
            seeds.seeds.insert(id, seed);
        }
    }

    /// Closes the changed external callees over the graph and reseeds each one.
    pub(super) fn seed_external_targets(
        &self,
        result: &mut UnitEffects,
        seeds: &mut DeltaSeeds,
        imports: &HashMap<String, String>,
        external: &impl Fn(&FunctionId) -> RuntimeEffects,
    ) {
        let (unit, queries) = (self.unit, self.queries);
        // Newly added owned callees may be discovered after their callers. Do not
        // overwrite their direct seed with a provisional external classification.
        seeds
            .external
            .retain(|id| !result.preemption_bodies.contains(id));
        let mut pending_external: Vec<_> = seeds.external.iter().copied().collect();
        while let Some(id) = pending_external.pop() {
            if let Some(sites) = self.graph.get(&id) {
                for &target in &sites.targets {
                    if !result.preemption_bodies.contains(&target) && seeds.external.insert(target)
                    {
                        pending_external.push(target);
                    }
                }
            }
        }
        for &target in &seeds.external {
            // An expression lambda may retain its previous block-lambda identity
            // without a graph entry of its own. A current incoming edge still
            // requires its external equation; it is not a deleted vertex.
            seeds.removed.remove(&target);
            if complete_dispatch_target(self.graph, self.callables, &target) {
                seeds.seeds.insert(target, EffectSummary::default());
                continue;
            }
            let mut seed = EffectSummary::new(
                external(&target).intersection(ANY_PANIC),
                Some(EffectWitness::External(target)),
            );
            if queries.external_capabilities(unit, &target, imports).may_io {
                seeds.io.insert(target);
            }
            if let Some(origin) = queries.external_target(unit, &target, imports) {
                if origin.0 != unit {
                    result.plan.providers.insert(origin.0);
                }
                result
                    .plan
                    .external
                    .entry(origin)
                    .or_default()
                    .insert(target);
            }
            if let Some((declaring, id, is_async)) =
                queries.external_lock_callable(unit, &target, imports)
                && !is_async
            {
                let evidence = queries.effect_evidence(declaring, id);
                let waiting = evidence.as_ref().map_or(LOCK_EFFECT_WAIT, |s| {
                    s.effects().intersection(LOCK_EFFECT_WAIT)
                });
                seed.add_effect(
                    waiting,
                    evidence.as_ref().and_then(|s| s.witness(LOCK_EFFECT_WAIT)),
                );
            }
            seeds.seeds.insert(target, seed);
        }
    }

    /// A cycle altered by an outgoing edge must contain the edited owner, so
    /// every potentially changed cycle member lies in its old reverse closure.
    pub(super) fn seed_helper_preemption(
        &self,
        result: &UnitEffects,
        solution: &crate::semantic::effects::EffectSolution<EffectWitness>,
        changed: &HashSet<FunctionId>,
        seeds: &mut DeltaSeeds,
    ) {
        let closure = solution.reverse_closure(changed.iter().copied());
        let mut helper_graph = CallGraph::default();
        for &id in &closure {
            if !result.plan.helpers.contains_key(&id) {
                continue;
            }
            if let Some(sites) = self.graph.get(&id) {
                let mut sites = sites.clone();
                sites.targets.retain(|target| {
                    closure.contains(target) && result.plan.helpers.contains_key(target)
                });
                helper_graph.merge(id, sites);
            }
        }
        let cycles = crate::semantic::effects::cycle_members(&helper_graph, std::iter::empty());
        for &id in &closure {
            if seeds.removed.contains(&id) {
                continue;
            }
            let seed = seeds
                .seeds
                .entry(id)
                .or_insert_with(|| solution.equation(id).unwrap().seed.clone());
            seed.remove_effect(NO_PREEMPT);
            if !result.plan.helpers.contains_key(&id) {
                continue;
            }
            let reason = if result.plan.loops.contains_key(&id) {
                Some(NonpreemptibleReason::Loop)
            } else if cycles.contains(&id) {
                Some(NonpreemptibleReason::Recursion)
            } else {
                None
            };
            if let Some(reason) = reason {
                let span = result
                    .plan
                    .loops
                    .get(&id)
                    .copied()
                    .or_else(|| self.body_span(&result.plan.bodies, id))
                    .unwrap_or(result.plan.helpers[&id]);
                seed.add_effect(
                    NO_PREEMPT,
                    Some(&EffectWitness::Helper {
                        reason,
                        owner: id,
                        source: (span.file_id.0, span.start, span.end),
                    }),
                );
            }
        }
    }

    pub(super) fn body_span(
        &self,
        bodies: &im::HashMap<FunctionId, BodyId>,
        id: FunctionId,
    ) -> Option<crate::diagnostics::Span> {
        bodies
            .get(&id)
            .and_then(|body| self.locations.get(&(self.unit, *body)))
            .and_then(|location| location.span(self.program))
    }

    /// Applies changed lock equations and forgets removed vertices. Returns
    /// the number of equations compared.
    pub(super) fn update_lock_equations(
        &self,
        result: &mut UnitEffects,
        solution: &mut crate::semantic::effects::EffectSolution<EffectWitness>,
        seeds: HashMap<FunctionId, EffectSummary<EffectWitness>>,
        removed: &HashSet<FunctionId>,
    ) -> usize {
        use crate::semantic::effects::EffectEquation;
        let mask = |target: &FunctionId| {
            let base = ANY_PANIC.union(RuntimeEffects::MAY_ALLOCATE);
            if self.callables.get(target) == Some(&true) {
                base
            } else if result.plan.helpers.contains_key(target) {
                base.union(LOCK_EFFECT_WAIT).union(NO_PREEMPT)
            } else {
                base.union(LOCK_EFFECT_WAIT)
            }
        };
        let mut updates = Vec::new();
        let mut compared = 0;
        for (id, seed) in seeds {
            compared += 1;
            let mut edges: Vec<_> = self
                .graph
                .get(&id)
                .map(|sites| {
                    sites
                        .targets
                        .iter()
                        .filter(|target| !removed.contains(target))
                        .map(|&target| (target, mask(&target)))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(copy) = result.plan.copies.get(&id)
                && copy.provider == self.unit
                && !edges.iter().any(|(target, _)| *target == copy.source)
            {
                edges.push((copy.source, mask(&copy.source)));
            }
            let equation = EffectEquation { seed, edges };
            if !solution.equation(id).is_some_and(|old| {
                old.edges == equation.edges && same_seed(&old.seed, &equation.seed)
            }) {
                updates.push((id, Some(equation)));
            }
        }
        for id in removed {
            result.plan.bodies.remove(id);
            result.plan.helpers.remove(id);
            result.callables.remove(id);
        }
        updates.extend(removed.iter().map(|&id| (id, None)));
        solution.apply_delta(updates);
        compared
    }

    /// Applies changed IO equations. Returns the number compared.
    pub(super) fn update_io_equations(
        &self,
        result: &UnitEffects,
        io_solution: &mut crate::semantic::effects::EffectSolution<u8>,
        changed: &HashSet<FunctionId>,
        io_seeds: &HashSet<FunctionId>,
        removed: &HashSet<FunctionId>,
    ) -> usize {
        use crate::semantic::effects::EffectEquation;
        let io_bit = RuntimeEffects::MAY_ALLOCATE;
        let mut io_updates = Vec::new();
        let mut compared = 0;
        for &id in changed {
            if removed.contains(&id) {
                io_updates.push((id, None));
                continue;
            }
            compared += 1;
            let mut equation = EffectEquation {
                seed: EffectSummary::new(
                    if io_seeds.contains(&id) {
                        io_bit
                    } else {
                        RuntimeEffects::NONE
                    },
                    None,
                ),
                edges: self
                    .graph
                    .get(&id)
                    .map(|sites| {
                        sites
                            .targets
                            .iter()
                            .filter(|target| !removed.contains(target))
                            .map(|&target| (target, io_bit))
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            if let Some(copy) = result.plan.copies.get(&id)
                && copy.provider == self.unit
                && !equation
                    .edges
                    .iter()
                    .any(|(target, _)| *target == copy.source)
            {
                equation.edges.push((copy.source, io_bit));
            }
            if !io_solution.equation(id).is_some_and(|old| old == &equation) {
                io_updates.push((id, Some(equation)));
            }
        }
        io_solution.apply_delta(io_updates);
        compared
    }

    /// Prunes previous external callees that no equation references any more.
    pub(super) fn prune_external_leaves(
        &self,
        result: &mut UnitEffects,
        solution: &mut crate::semantic::effects::EffectSolution<EffectWitness>,
        io_solution: &mut crate::semantic::effects::EffectSolution<u8>,
        old_external_targets: HashSet<FunctionId>,
        imports: &HashMap<String, String>,
    ) {
        for target in old_external_targets {
            if !result.preemption_bodies.contains(&target)
                && solution.prune_unreferenced_leaf(target)
            {
                io_solution.prune_unreferenced_leaf(target);
                if let Some(origin) = self.queries.external_target(self.unit, &target, imports)
                    && let Some(targets) = result.plan.external.get_mut(&origin)
                {
                    targets.remove(&target);
                    if targets.is_empty() {
                        result.plan.external.remove(&origin);
                    }
                }
            }
        }
    }

    /// Projects the affected facts into the unit result and retains both
    /// solutions for the next delta.
    pub(super) fn publish(
        &self,
        mut result: UnitEffects,
        mut solution: crate::semantic::effects::EffectSolution<EffectWitness>,
        io_solution: crate::semantic::effects::EffectSolution<u8>,
        changed: &HashSet<FunctionId>,
        compared: usize,
    ) -> UnitEffects {
        let (queries, unit) = (self.queries, self.unit);
        let io_bit = RuntimeEffects::MAY_ALLOCATE;
        for &id in &io_solution.affected {
            if io_solution
                .facts
                .get(&id)
                .is_some_and(|s| s.intersects(io_bit))
            {
                result.may_io.insert(id);
            } else {
                result.may_io.remove(&id);
            }
        }
        let helpers = Arc::make_mut(&mut result.helpers);
        for &id in &solution.affected {
            if let Some(&span) = result.plan.helpers.get(&id)
                && let Some(EffectWitness::Helper { reason, .. }) =
                    solution.facts.get(&id).and_then(|s| s.witness(NO_PREEMPT))
            {
                let span = self.body_span(&result.plan.bodies, id).unwrap_or(span);
                helpers.insert(
                    id,
                    NonpreemptibleHelper {
                        span,
                        reason: *reason,
                    },
                );
            } else {
                helpers.remove(&id);
            }
        }
        solution.affected.extend(changed.iter().copied());
        queries.compared.set(queries.compared.get() + compared);
        queries
            .recomputed
            .set(queries.recomputed.get() + solution.recomputed + io_solution.recomputed);
        result.facts = solution.facts.clone();
        queries
            .solutions
            .borrow_mut()
            .insert(unit, Arc::new(solution));
        queries
            .io_solutions
            .borrow_mut()
            .insert(unit, Arc::new(io_solution));
        result
    }
}

/// Removes a deleted root's inherited and copy contributions.
pub(super) fn forget_removed_root(result: &mut UnitEffects, root: BodyId) {
    if let Some(copy) = result.plan.inherited.remove(&root)
        && let Some(targets) = result
            .plan
            .inherited_targets
            .get_mut(&(copy.provider, copy.source))
    {
        if let Some(ids) = result.plan.roots.get(&root) {
            for id in ids.iter() {
                targets.remove(id);
            }
        }
        if targets.is_empty() {
            result
                .plan
                .inherited_targets
                .remove(&(copy.provider, copy.source));
        }
    }

    if let Some(ids) = result.plan.roots.remove(&root) {
        for &id in ids.iter() {
            if let Some(copy) = result.plan.copies.remove(&id) {
                result
                    .preemption_copies
                    .remove(&(id, copy.provider, copy.source));
                if let Some(targets) = result
                    .plan
                    .copy_targets
                    .get_mut(&(copy.provider, copy.source))
                {
                    targets.remove(&id);
                    if targets.is_empty() {
                        result
                            .plan
                            .copy_targets
                            .remove(&(copy.provider, copy.source));
                        if let Some(sources) = result.plan.copy_sources.get_mut(&copy.provider) {
                            sources.remove(&copy.source);
                        }
                    }
                }
            }
            result.plan.helpers.remove(&id);
            result.callables.remove(&id);
        }
    }
}
