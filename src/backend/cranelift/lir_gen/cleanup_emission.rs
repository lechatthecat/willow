//! Bind immutable LIR cleanup states to machine resources once, before emission.
use super::*;
use crate::ir::lowered::cleanup::{CleanupGraph, CleanupStateId};
use std::rc::Rc;

impl LirCleanupBindings {
    pub(super) fn state(&self, state: Option<CleanupStateId>) -> &LirCleanupState {
        state.map_or(&self.initial, |id| &self.states[id.0])
    }
}

impl FuncGen<'_, '_> {
    pub(super) fn install_lir_cleanup(&mut self, state: &LirCleanupState) {
        self.defer_stack = state.entries.clone();
        self.panic_scopes = state.panic.clone();
        self.lock_scopes = state.locks.clone();
    }

    pub(super) fn prepare_lir_cleanup(
        &mut self,
        function: &LirFunction,
        graph: &CleanupGraph,
        blocks: &[cranelift_codegen::ir::Block],
        cooperative: bool,
    ) -> LirCleanupBindings {
        let initial = LirCleanupState {
            entries: self.defer_stack.clone(),
            panic: self.panic_scopes.clone(),
            locks: self.lock_scopes.clone(),
        };
        let roots = self.gc_root_count;
        let variables = Rc::new(self.vars.clone());
        let bindings = Rc::new(if cooperative {
            self.vars
                .iter()
                .filter(|(_, storage)| {
                    matches!(
                        storage,
                        VarStorage::Frame { .. } | VarStorage::FrameReference { .. }
                    )
                })
                .map(|(name, storage)| (name.clone(), storage.clone()))
                .collect()
        } else {
            Vec::new()
        });
        let mut sites = HashMap::new();
        for block in &function.blocks {
            for instruction in &block.instrs {
                if let LirInst::Defer { id, body, .. } = instruction {
                    let slot = (!cooperative).then(|| {
                        self.builder.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            8,
                            0,
                        ))
                    });
                    if let Some(slot) = slot {
                        self.sync_defer_flags.insert(*id, slot);
                    }
                    let entry = Rc::new(super::super::DeferEntry {
                        id: self.defer_counter,
                        action: Rc::new(body.clone()),
                        flag_offset: cooperative.then(|| self.lir_defer_offsets[id]),
                        sync_flag_slot: slot,
                        bindings: bindings.clone(),
                        vars_at_registration: variables.clone(),
                        recovery_capable: body.contains_recover(),
                    });
                    self.defer_counter += 1;
                    sites.insert(*id, entry);
                }
            }
        }
        let sites = Rc::new(sites);
        let cleanups: Vec<_> = graph
            .scopes
            .iter()
            .map(|_| self.builder.create_block())
            .collect();
        let mut depths = Vec::with_capacity(graph.states.len());
        for state in &graph.states {
            let depth = state.parent.map_or(1, |id| depths[id.0] + 1);
            depths.push(depth);
        }
        let mut scopes = Vec::with_capacity(graph.scopes.len());
        let mut scope_locks = Vec::with_capacity(graph.scopes.len());
        for (index, scope) in graph.scopes.iter().enumerate() {
            let defer_depth = initial.entries.len() + depths[scope.entry_state.unwrap().0] - 1;
            let root_depth_at_entry = if cooperative {
                super::super::RootDepth::Value(
                    self.panic_function_root_depth.expect("poll root depth"),
                )
            } else {
                let var = self.builder.declare_var(types::I32);
                // Entry initialization also gives dead cleanup blocks a definition.
                let zero = self.builder.ins().iconst(types::I32, 0);
                self.builder.def_var(var, zero);
                super::super::RootDepth::Variable(var)
            };
            let LirInst::EnterDeferScope { lock, .. } =
                &function.blocks[scope.location.0].instrs[scope.location.1]
            else {
                unreachable!()
            };
            let lock = lock.as_ref().map(|slots| {
                let offsets = self.lir_lock_offsets(slots);
                super::super::CoopLockScope {
                    mode: slots.mode,
                    handle_offset: offsets[0],
                    token_offset: offsets[1],
                    phase_offset: offsets[2],
                    value_offset: offsets[3],
                    value_ty: slots.value_ty.clone(),
                    defer_depth,
                }
            });
            scope_locks.push(lock);
            scopes.push(super::super::PanicScope {
                call_frames_at_entry: Vec::new(),
                reference_scopes_at_entry: Vec::new(),
                cleanup: cleanups[index],
                resume: scope
                    .recovery
                    .map_or_else(|| self.builder.create_block(), |id| blocks[id.0]),
                root_depth_at_entry,
                defer_depth,
                vars_before: variables.clone(),
                coop_root_depth_at_entry: cooperative.then(|| self.coop_root_depth()),
                program: Rc::new(scope.panic_program.clone()),
                entries: sites.clone(),
                parent_cleanup: scope
                    .parent
                    .map(|id| cleanups[id.0])
                    .or_else(|| initial.panic.last().map(|scope| scope.cleanup)),
            });
        }
        // Scope registration order is a lexical property, not emission order.
        // The graph walk visits an ancestor before its nested scope.
        if cooperative {
            for &block in &graph.order {
                for (position, _) in function.blocks[block].instrs.iter().enumerate() {
                    let Some(id) = graph.enters.get(&(block, position)) else {
                        continue;
                    };
                    if let Some(lock) = &scope_locks[id.0] {
                        let order = self.next_cleanup_order();
                        self.collected_lock_sites.push(super::super::AsyncLockSite {
                            mode: lock.mode,
                            handle_offset: lock.handle_offset,
                            token_offset: lock.token_offset,
                            phase_offset: lock.phase_offset,
                            value_offset: lock.value_offset,
                            value_ty: lock.value_ty.clone(),
                            order,
                        });
                    }
                    for site in &graph.scopes[id.0].sites {
                        let entry = &sites[site];
                        let order = self.next_cleanup_order();
                        self.collected_defer_sites
                            .push(super::super::AsyncDeferSite {
                                action: entry.action.clone(),
                                flag_offset: entry.flag_offset.unwrap(),
                                bindings: entry.bindings.clone(),
                                recovery_capable: entry.recovery_capable,
                                order,
                            });
                    }
                }
            }
        }
        let mut states: Vec<LirCleanupState> = Vec::with_capacity(graph.states.len());
        let mut registration_states = HashMap::new();
        for (index, state) in graph.states.iter().enumerate() {
            let machine = if let Some(registration) = state.registrations {
                let node = &graph.registrations[registration.0];
                let previous = node
                    .previous
                    .map_or(graph.scopes[state.scope.0].entry_state.unwrap().0, |id| {
                        registration_states[&id]
                    });
                let mut machine = states[previous].clone();
                if !cooperative {
                    machine.entries.register(sites[&node.site].clone());
                }
                registration_states.insert(registration, index);
                machine
            } else {
                let mut machine = state.parent.map_or(&initial, |id| &states[id.0]).clone();
                machine.entries.push(if cooperative {
                    graph.scopes[state.scope.0]
                        .sites
                        .iter()
                        .map(|id| sites[id].clone())
                        .collect()
                } else {
                    Vec::new()
                });
                machine.panic.push(scopes[state.scope.0].clone());
                if let Some(lock) = &scope_locks[state.scope.0] {
                    machine.locks.push(lock.clone());
                }
                machine
            };
            states.push(machine);
        }
        let mut panic_states = Vec::with_capacity(graph.scopes.len());
        for (index, scope) in graph.scopes.iter().enumerate() {
            let mut state = scope
                .parent_state
                .map_or(&initial, |id| &states[id.0])
                .clone();
            state.entries.push_guarded(
                scope.sites.iter().map(|id| sites[id].clone()).collect(),
                true,
            );
            state.panic.push(scopes[index].clone());
            if let Some(lock) = &scope_locks[index] {
                state.locks.push(lock.clone());
            }
            panic_states.push(state);
        }
        LirCleanupBindings {
            initial,
            states,
            scopes,
            panic_states,
            normal: HashMap::new(),
            roots,
        }
    }
}
