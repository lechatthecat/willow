use super::*;

/// Delta stages, in the order `delta_snapshot` runs them.
impl VerificationSnapshotState {
    /// Resets per-revision counters and rejects revisions a delta cannot patch.
    pub(super) fn begin_delta(
        &mut self,
        frontend: &crate::Frontend,
        plan: &Plan,
    ) -> Result<SnapshotOutcome<()>> {
        self.projection_items_cloned.set(0);
        self.boundary_functions.set(0);
        self.owner_checks = plan.owner_checks;
        self.coordinate_owners = plan.current_roots.len() + plan.declarations.len();
        if let Some(retry) = plan.retry {
            return Ok(SnapshotOutcome::Retry(retry));
        }
        if self
            .dispatch
            .as_ref()
            .is_some_and(|dispatch| !dispatch.supported())
            || frontend.module_graph.files.len() + 1 != self.units.len()
        {
            return Ok(SnapshotOutcome::Retry(SnapshotRetry::FullCapture));
        }
        for key in &plan.changed {
            if !self.keys.contains_key(key) {
                let path = &self.units[&key.0].path;
                self.keys
                    .insert(*key, hash(serde_json::to_vec(&(path, key.1.to_string()))?));
            }
        }
        Ok(SnapshotOutcome::Done(()))
    }

    /// Removed owners with their descendants, and the reverse-call closure of
    /// every changed, removed, effect-affected or indirect-call-affected owner.
    pub(super) fn affected_closure(
        &mut self,
        frontend: &crate::Frontend,
        plan: &Plan,
        retired_bodies: &HashSet<BodyId>,
    ) -> Result<SnapshotOutcome<DeltaClosure>> {
        let mut removed_functions: Vec<_> = plan
            .removed
            .iter()
            .filter_map(|key| self.keys.get(key).cloned())
            .collect();
        for key in &plan.removed {
            if let Some(id) = self.keys.get(key)
                && let Some(body) = self
                    .records
                    .get(id)
                    .and_then(|record| record.function.body_id)
                && let Some(descendants) = self.descendants.get(&body)
            {
                removed_functions.extend(descendants.iter().cloned());
            }
        }
        removed_functions.sort();
        removed_functions.dedup();
        let removed_ids: HashSet<_> = removed_functions.into_iter().collect();
        let mut affected = HashSet::new();
        let mut queue = VecDeque::new();
        let indirect_callers = self.replace_indirect_inputs(
            frontend,
            plan,
            retired_bodies,
            &mut affected,
            &mut queue,
        )?;
        for key in plan.changed.iter().chain(&plan.removed) {
            let Some(id) = self.keys.get(key).cloned() else {
                return Ok(SnapshotOutcome::Retry(SnapshotRetry::FullCapture));
            };
            if affected.insert(id.clone()) {
                queue.push_back(id);
            }
        }
        for unit in self.units.keys() {
            for function in frontend.db.effects.affected_functions(*unit) {
                if let Some(id) = self.keys.get(&(*unit, function))
                    && affected.insert(id.clone())
                {
                    queue.push_back(id.clone());
                }
            }
        }
        while let Some(id) = queue.pop_front() {
            for caller in self.reverse.get(&id).into_iter().flatten() {
                if affected.insert(caller.clone()) {
                    queue.push_back(caller.clone());
                }
            }
        }
        affected.retain(|id| !removed_ids.contains(id));
        Ok(SnapshotOutcome::Done(DeltaClosure {
            affected,
            removed_ids,
            indirect_callers,
        }))
    }

    /// Replaces the indirect-call inputs of retired, re-projected, changed and
    /// removed bodies. Seeds the descendants of changed and removed bodies and
    /// the callers whose indirect targets changed; returns those callers.
    pub(super) fn replace_indirect_inputs(
        &mut self,
        frontend: &crate::Frontend,
        plan: &Plan,
        retired_bodies: &HashSet<BodyId>,
        affected: &mut HashSet<String>,
        queue: &mut VecDeque<String>,
    ) -> Result<HashSet<Key>> {
        let mut indirect_callers = HashSet::new();
        for body in retired_bodies {
            if let Some(root) = self.roots.get(body) {
                indirect_callers.extend(self.indirect.replace(root.key.0, *body, None));
            }
        }
        for body in &plan.projected_bodies {
            if let Some(root) = plan.current_roots.get(body)
                && plan.current_keys.get(&root.key) != Some(body)
            {
                indirect_callers.extend(self.indirect.replace(
                    root.key.0,
                    *body,
                    Some(indirect_inputs_tree(&frontend.db.typed_bodies, *body)?),
                ));
            }
        }
        for key in plan.changed.iter().chain(&plan.removed) {
            let body = plan.current_keys.get(key).copied().or_else(|| {
                self.keys
                    .get(key)
                    .and_then(|id| self.records.get(id))
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
                indirect_callers.extend(self.indirect.replace(key.0, body, inputs));
                if let Some(ids) = self.descendants.get(&body) {
                    for id in ids {
                        if affected.insert(id.clone()) {
                            queue.push_back(id.clone());
                        }
                    }
                }
            }
        }
        for caller in &indirect_callers {
            if let Some(id) = self.keys.get(caller)
                && affected.insert(id.clone())
            {
                queue.push_back(id.clone());
            }
        }
        Ok(indirect_callers)
    }

    /// Moves root and declaration anchors to their current spans and returns
    /// the coordinate changes of owners that moved.
    pub(super) fn relocate_owners(&mut self, plan: &Plan) -> Vec<VerificationRelocation> {
        let mut relocations = Vec::new();
        for (body, root) in &plan.current_roots {
            if let Some(old) = self.roots.get(body)
                && old.span != root.span
            {
                relocations.push(VerificationRelocation {
                    path: self.units[&old.key.0].path.clone(),
                    old_start: old.span.start,
                    old_end: old.span.end,
                    new_start: root.span.start,
                    new_end: root.span.end,
                });
            }
            self.anchors.insert(
                root_anchor(*body),
                Location {
                    display: None,
                    path: self.units[&root.key.0].path.clone(),
                    start: root.span.start,
                    end: root.span.end,
                },
            );
            self.roots.insert(*body, root.clone());
        }
        for (key, span) in &plan.declarations {
            if let Some(old) = self.declarations.get(key)
                && old != span
            {
                relocations.push(VerificationRelocation {
                    path: self.units[&key.0].path.clone(),
                    old_start: old.start,
                    old_end: old.end,
                    new_start: span.start,
                    new_end: span.end,
                });
            }
            self.anchors.insert(
                declaration_anchor(key),
                Location {
                    display: None,
                    path: self.units[&key.0].path.clone(),
                    start: span.start,
                    end: span.end,
                },
            );
            self.declarations.insert(key.clone(), *span);
        }
        relocations
    }

    /// Projection ids of changed owners and their current descendants.
    pub(super) fn changed_ids(&self, plan: &Plan) -> HashSet<String> {
        let mut changed_ids: HashSet<_> = plan
            .changed
            .iter()
            .filter_map(|key| self.keys.get(key).cloned())
            .collect();
        for key in &plan.changed {
            if let Some(body) = plan.current_keys.get(key)
                && let Some(ids) = self.descendants.get(body)
            {
                changed_ids.extend(ids.iter().cloned());
            }
        }
        changed_ids
    }

    /// Records current bodyless types and drops declarations that a rechecked
    /// unit no longer declares.
    pub(super) fn retire_declarations(&mut self, plan: &Plan) {
        for (key, is_enum) in &plan.bodyless {
            self.bodyless.insert(key.clone(), *is_enum);
        }
        for key in plan
            .pending_declarations
            .keys()
            .filter(|key| plan.unit_items.contains_key(&key.0))
        {
            self.declarations.remove(key);
            self.anchors.remove(&declaration_anchor(key));
            if let Some((_, name)) = key.1.split_once(':') {
                self.bodyless.remove(&(key.0, name.to_owned()));
            }
        }
    }

    /// Marks patch functions under changed roots as changed and retires the old
    /// descendants of changed roots that the patch no longer produces.
    pub(super) fn mark_changed_projections(
        &mut self,
        frontend: &crate::Frontend,
        plan: &Plan,
        patch: &Snapshot,
        affected: &mut HashSet<String>,
        changed_ids: &mut HashSet<String>,
        removed_ids: &mut HashSet<String>,
    ) {
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
                        self.keys.insert(key, function.id.clone());
                    }
                }
            }
            if matches!(
                function.synthetic_kind,
                crate::qualified_name::SyntheticKind::Indirect { .. }
            ) {
                affected.insert(function.id.clone());
            }
        }
        for key in &plan.changed {
            if let Some(body) = plan.current_keys.get(key)
                && let Some(old) = self.descendants.get(body)
            {
                for id in old {
                    if !current_changed.contains(id) {
                        removed_ids.insert(id.clone());
                    }
                }
            }
        }
    }

    /// Removes indirect-arity buckets reachable from changed or removed owners
    /// that no longer have an active indirect call.
    pub(super) fn retire_indirect_buckets(
        &self,
        changed_ids: &HashSet<String>,
        removed_ids: &mut HashSet<String>,
    ) {
        let mut bucket_queue: Vec<_> = changed_ids
            .iter()
            .chain(removed_ids.iter())
            .filter_map(|id| self.records.get(id))
            .flat_map(|record| {
                record
                    .function
                    .callees
                    .iter()
                    .filter(|id| self.is_indirect_bucket(id))
                    .cloned()
            })
            .collect();
        let mut seen_buckets = HashSet::new();
        while let Some(id) = bucket_queue.pop() {
            if !seen_buckets.insert(id.clone()) {
                continue;
            }
            if !self.indirect.active(
                &id,
                self.indirect_kinds.get(&id).copied().unwrap_or_default(),
            ) {
                removed_ids.insert(id.clone());
            }
            if let Some(record) = self.records.get(&id) {
                bucket_queue.extend(
                    record
                        .function
                        .callees
                        .iter()
                        .filter(|id| self.is_indirect_bucket(id))
                        .cloned(),
                );
            }
        }
    }

    /// Omits reference types the client already holds for the same target.
    pub(super) fn elide_unchanged_reference_types(&self, patch: &mut Snapshot) {
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
                    .or_else(|| self.symbol_types.get(&reference.target).map(AsRef::as_ref))
                    == Some(&reference.ty)
            {
                reference.ty = None;
            }
        }
    }

    /// Symbols of changed or removed owners that the patch no longer declares.
    pub(super) fn removed_symbols(
        &self,
        patch: &Snapshot,
        changed_ids: &HashSet<String>,
        removed_ids: &HashSet<String>,
    ) -> Vec<String> {
        let present_symbols: HashSet<_> = patch
            .semantic
            .symbols
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        changed_ids
            .iter()
            .chain(removed_ids)
            .filter_map(|id| self.records.get(id))
            .flat_map(|record| record.semantic.symbols.iter().map(|s| s.id.clone()))
            .filter(|id| !present_symbols.contains(id.as_str()))
            .collect()
    }

    /// Effect-only projections reuse their body-local semantic data; only the
    /// solved effect value and source coordinates are refreshed.
    pub(super) fn merge_effect_only_projections(
        &self,
        frontend: &crate::Frontend,
        patch: &mut Snapshot,
        changed_ids: &HashSet<String>,
    ) {
        for function in &patch.functions {
            if changed_ids.contains(&function.id) || function.body_id.is_none() {
                continue;
            }
            let mut cached = self.current_record(&function.id, true);
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
    }

    /// Drops removed projections with their call edges, keys and root anchors.
    pub(super) fn remove_records(&mut self, removed_functions: &[String]) {
        for id in removed_functions {
            self.dirty_instance_owners.insert(id.clone());
            self.indirect_kinds.remove(id);
            if let Some(old) = self.records.remove(id) {
                for callee in &old.function.callees {
                    if let Some(callers) = self.reverse.get_mut(callee) {
                        callers.remove(id);
                    }
                }
                if let Some(body) = old.function.body_id {
                    if let Some(key) = old.key
                        && self.keys.get(&key).is_some_and(|current| current == id)
                    {
                        self.keys.remove(&key);
                        self.own.remove(&key);
                    }
                    let root = old.root.unwrap_or(body);
                    if let Some(ids) = self.descendants.get_mut(&root) {
                        ids.remove(id);
                    }
                    if self
                        .descendants
                        .get(&root)
                        .is_some_and(|ids| ids.is_empty())
                    {
                        self.descendants.remove(&root);
                    }
                    self.roots.remove(&body);
                    self.anchors.remove(&root_anchor(body));
                }
            }
            self.reverse.remove(id);
            self.bytes = self
                .bytes
                .saturating_sub(self.encoded.remove(id).unwrap_or(0));
        }
    }

    /// Stores patched projections, keeping retained anchors and order, and
    /// replaces their reverse call edges and encoded sizes.
    pub(super) fn commit_records(
        &mut self,
        frontend: &crate::Frontend,
        records: HashMap<String, Projection>,
        body_locations: &HashMap<BodyId, usize>,
    ) -> Result<()> {
        for (id, mut record) in records {
            bind_record_owner(frontend, &mut record, body_locations);
            record.anchor = self
                .records
                .get(&id)
                .and_then(|old| old.anchor.as_ref())
                .and_then(|(key, _)| {
                    self.anchors
                        .get(key)
                        .map(|location| (key.clone(), location.clone()))
                })
                .or_else(|| {
                    record.function.body_id.and_then(|body| {
                        let key = root_anchor(frontend.db.bodies().root_body(body));
                        self.anchors
                            .get(&key)
                            .map(|location| (key.clone(), location.clone()))
                    })
                });
            if let Some(old) = self.records.get(&id) {
                record.order = old.order;
            }
            if let Some(old) = self.records.get(&id) {
                for callee in &old.function.callees {
                    if let Some(callers) = self.reverse.get_mut(callee) {
                        callers.remove(&id);
                    }
                }
            }
            for callee in &record.function.callees {
                self.reverse
                    .entry(callee.clone())
                    .or_default()
                    .insert(id.clone());
            }
            let size = encoded(&record)?;
            self.bytes = self
                .bytes
                .saturating_sub(self.encoded.get(&id).copied().unwrap_or(0))
                + size;
            self.encoded.insert(id.clone(), size);
            if let Some(body) = record.function.body_id {
                self.descendants
                    .entry(frontend.db.bodies().root_body(body))
                    .or_default()
                    .insert(id.clone());
            }
            self.dirty_instance_owners.insert(id.clone());
            self.record_indirect_kind(&id, record.function.synthetic_kind);
            self.records.insert(id, Arc::new(record));
        }
        Ok(())
    }

    /// Forgets roots the revision removed, keeping keys a current root reuses.
    pub(super) fn retire_bodies(&mut self, plan: &Plan, retired_bodies: HashSet<BodyId>) {
        for body in retired_bodies {
            if let Some(root) = self.roots.remove(&body)
                && !plan.current_keys.contains_key(&root.key)
            {
                self.keys.remove(&root.key);
                self.own.remove(&root.key);
            }
            self.anchors.remove(&root_anchor(body));
            self.descendants.remove(&body);
        }
    }
}
