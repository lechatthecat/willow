//! Function-local cleanup control-flow analysis, independent of backend order.
//!
//! Scope stacks are persistent nodes: a CFG edge carries one identity, rather
//! than copying all its ancestors. Normal and recovery edges must agree before
//! any machine blocks are emitted.
use super::{BlockId, LirDeferId, LirFunction, LirInst, Terminator};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RootScopeId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CleanupRegionId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CleanupStateId(pub usize);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegistrationId(pub usize);
#[derive(Debug)]
pub struct State {
    pub scope: RootScopeId,
    pub parent: Option<CleanupStateId>,
    pub registrations: Option<RegistrationId>,
}
#[derive(Debug)]
pub struct Registration {
    pub site: LirDeferId,
    pub previous: Option<RegistrationId>,
}
#[derive(Debug, Clone, Copy)]
pub enum CleanupTransition {
    Enter {
        scope: RootScopeId,
        state: Option<CleanupStateId>,
    },
    Register {
        site: LirDeferId,
        state: Option<CleanupStateId>,
    },
    Exit {
        edge: CleanupEdge,
        state: Option<CleanupStateId>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanicTarget {
    Scope(RootScopeId),
    Propagate,
}

/// Executable panic cleanup CFG. Block zero is the entry; site guards consume
/// their registration before invocation so a panic cannot run a defer twice.
#[derive(Debug, Default, Clone)]
pub struct PanicProgram {
    pub blocks: Vec<PanicBlock>,
}
#[derive(Debug, Clone)]
pub struct PanicBlock {
    pub instructions: Vec<PanicInstruction>,
    pub terminator: PanicTerminator,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanicInstruction {
    Consume(LirDeferId),
    Invoke(LirDeferId),
    ReleaseLock(RootScopeId),
    RestoreRoots(RootScopeId),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanicTerminator {
    IfRegistered {
        site: LirDeferId,
        run: usize,
        next: usize,
    },
    Jump(usize),
    Dispatch {
        scope: RootScopeId,
        recovery: Option<BlockId>,
        propagate: PanicTarget,
    },
}

impl PanicProgram {
    fn lower(
        scope: RootScopeId,
        sites: &[LirDeferId],
        lock: bool,
        recovery: Option<BlockId>,
        propagate: PanicTarget,
    ) -> Self {
        let mut blocks = Vec::with_capacity(2 * sites.len() + 1);
        for &site in sites.iter().rev() {
            let check = blocks.len();
            blocks.push(PanicBlock {
                instructions: vec![],
                terminator: PanicTerminator::IfRegistered {
                    site,
                    run: check + 1,
                    next: check + 2,
                },
            });
            blocks.push(PanicBlock {
                instructions: vec![
                    PanicInstruction::Consume(site),
                    PanicInstruction::Invoke(site),
                ],
                terminator: PanicTerminator::Jump(check + 2),
            });
        }
        let mut instructions = Vec::with_capacity(2);
        if lock {
            instructions.push(PanicInstruction::ReleaseLock(scope));
        }
        instructions.push(PanicInstruction::RestoreRoots(scope));
        blocks.push(PanicBlock {
            instructions,
            terminator: PanicTerminator::Dispatch {
                scope,
                recovery,
                propagate,
            },
        });
        Self { blocks }
    }
}

#[derive(Debug)]
pub struct Scope {
    pub panic_program: PanicProgram,
    pub location: (usize, usize),
    pub entry_state: Option<CleanupStateId>,
    pub parent_state: Option<CleanupStateId>,
    pub parent: Option<RootScopeId>,
    pub sites: Vec<LirDeferId>,
    pub recovery: Option<BlockId>,
    pub panic: PanicTarget,
    site_set: HashSet<LirDeferId>,
}

#[derive(Debug, Clone, Copy)]
pub struct CleanupEdge {
    pub region: CleanupRegionId,
    pub from: Option<RootScopeId>,
    pub to: Option<RootScopeId>,
    pub scopes: usize,
}

/// Explicit scope transitions attached to instruction positions. IDs are local
/// to this graph, never source spans or backend block numbers.
#[derive(Debug, Default)]
pub struct CleanupGraph {
    pub states: Vec<State>,
    pub registrations: Vec<Registration>,
    pub incoming_states: Vec<Option<CleanupStateId>>,
    pub before: HashMap<(usize, usize), Option<CleanupStateId>>,
    pub after: HashMap<(usize, usize), Option<CleanupStateId>>,
    pub transitions: HashMap<(usize, usize), CleanupTransition>,
    /// Number of CFG edges checked; each check compares one identity.
    pub edge_checks: usize,
    pub scopes: Vec<Scope>,
    pub entries: Vec<Option<RootScopeId>>,
    pub enters: HashMap<(usize, usize), RootScopeId>,
    pub exits: HashMap<(usize, usize), CleanupEdge>,
    pub regions: usize,
    pub order: Vec<usize>,
}

impl CleanupGraph {
    pub fn lower(function: &LirFunction) -> Result<Self, &'static str> {
        let mut graph = Self {
            entries: vec![None; function.blocks.len()],
            incoming_states: vec![None; function.blocks.len()],
            ..Self::default()
        };
        if function.blocks.is_empty() {
            return Ok(graph);
        }
        let mut site_owners = HashMap::new();
        let mut registrations = HashSet::new();
        for (block_index, block) in function.blocks.iter().enumerate() {
            if block.id.0 != block_index {
                return Err("invalid cleanup block identity");
            }
            for (position, instruction) in block.instrs.iter().enumerate() {
                if let LirInst::EnterDeferScope { sites, resume, .. } = instruction {
                    let id = RootScopeId(graph.scopes.len());
                    for (site, _) in sites {
                        if site_owners
                            .insert(*site, id)
                            .is_some_and(|owner| owner != id)
                        {
                            return Err("defer site is owned by multiple cleanup scopes");
                        }
                    }
                    let mut site_set = HashSet::new();
                    let unique_sites = sites
                        .iter()
                        .filter_map(|(site, _)| site_set.insert(*site).then_some(*site))
                        .collect();
                    graph.enters.insert((block_index, position), id);
                    graph.scopes.push(Scope {
                        panic_program: PanicProgram::default(),
                        location: (block_index, position),
                        entry_state: None,
                        parent_state: None,
                        parent: None,
                        sites: unique_sites,
                        recovery: *resume,
                        panic: PanicTarget::Propagate,
                        site_set,
                    });
                }
                if let LirInst::Defer { id, .. } = instruction
                    && !registrations.insert(*id)
                {
                    return Err("duplicate defer registration instruction");
                }
            }
        }
        let has_scopes = !graph.scopes.is_empty();
        let mut visited = vec![false; function.blocks.len()];
        let mut pending = vec![0];
        visited[0] = true;
        let mut regions = HashMap::new();
        while let Some(index) = pending.pop() {
            graph.order.push(index);
            let mut state = graph.incoming_states[index];
            let mut scope = state.map(|id| graph.states[id.0].scope);
            graph.entries[index] = scope;
            for (position, instruction) in function.blocks[index].instrs.iter().enumerate() {
                let location = (index, position);
                let cleanup = matches!(
                    instruction,
                    LirInst::EnterDeferScope { .. }
                        | LirInst::Defer { .. }
                        | LirInst::LeaveDeferScope { .. }
                        | LirInst::FlushDefers { .. }
                );
                if cleanup {
                    graph.before.insert(location, state);
                }
                match instruction {
                    LirInst::EnterDeferScope { resume, lock, .. } => {
                        let id = graph.enters[&(index, position)];
                        graph.scopes[id.0].parent_state = state;
                        graph.scopes[id.0].parent = scope;
                        graph.scopes[id.0].panic =
                            scope.map_or(PanicTarget::Propagate, PanicTarget::Scope);
                        graph.scopes[id.0].panic_program = PanicProgram::lower(
                            id,
                            &graph.scopes[id.0].sites,
                            lock.is_some(),
                            *resume,
                            graph.scopes[id.0].panic,
                        );
                        if let Some(target) = resume {
                            graph.edge_checks += 1;
                            propagate(
                                target.0,
                                state,
                                &mut graph.incoming_states,
                                &mut visited,
                                &mut pending,
                            )?;
                        }
                        let next = CleanupStateId(graph.states.len());
                        graph.states.push(State {
                            scope: id,
                            parent: state,
                            registrations: None,
                        });
                        state = Some(next);
                        graph.scopes[id.0].entry_state = state;
                        scope = Some(id);
                        graph
                            .transitions
                            .insert(location, CleanupTransition::Enter { scope: id, state });
                    }
                    LirInst::LeaveDeferScope { sites } => {
                        let from_state = state;
                        let from = scope.ok_or("cleanup scope underflow")?;
                        let requested: HashSet<_> = sites.iter().copied().collect();
                        if requested != graph.scopes[from.0].site_set {
                            return Err("cleanup leave sites disagree with current scope");
                        }
                        state = graph.states[state.expect("active scope has state").0].parent;
                        scope = state.map(|id| graph.states[id.0].scope);
                        let next = regions.len();
                        let region = *regions
                            .entry((from_state, state))
                            .or_insert(CleanupRegionId(next));
                        graph.exits.insert(
                            (index, position),
                            CleanupEdge {
                                region,
                                from: Some(from),
                                to: scope,
                                scopes: 1,
                            },
                        );
                    }
                    LirInst::FlushDefers { sites, scopes } => {
                        let requested: HashSet<_> = sites.iter().copied().collect();
                        let mut covered = HashSet::new();
                        let from = scope;
                        let from_state = state;
                        for _ in 0..*scopes {
                            let id = scope.ok_or("cleanup scope underflow")?;
                            covered.extend(graph.scopes[id.0].site_set.iter().copied());
                            state = graph.states[state.expect("active scope has state").0].parent;
                            scope = state.map(|id| graph.states[id.0].scope);
                        }
                        if requested != covered {
                            return Err(
                                "cleanup flush sites are not a complete active scope suffix",
                            );
                        }
                        let next = regions.len();
                        let region = *regions
                            .entry((from_state, state))
                            .or_insert(CleanupRegionId(next));
                        graph.exits.insert(
                            (index, position),
                            CleanupEdge {
                                region,
                                from,
                                to: scope,
                                scopes: *scopes,
                            },
                        );
                    }
                    LirInst::Defer { id, .. } => {
                        let current = scope.ok_or("defer outside a cleanup scope")?;
                        if !graph.scopes[current.0].site_set.contains(id) {
                            return Err("defer site is not owned by current cleanup scope");
                        }
                        let previous = &graph.states[state.expect("active scope has state").0];
                        let registration = RegistrationId(graph.registrations.len());
                        graph.registrations.push(Registration {
                            site: *id,
                            previous: previous.registrations,
                        });
                        let next = CleanupStateId(graph.states.len());
                        graph.states.push(State {
                            scope: current,
                            parent: previous.parent,
                            registrations: Some(registration),
                        });
                        state = Some(next);
                        graph
                            .transitions
                            .insert(location, CleanupTransition::Register { site: *id, state });
                    }
                    _ => {}
                }
                if cleanup {
                    graph.after.insert(location, state);
                    if let Some(&edge) = graph.exits.get(&location) {
                        graph
                            .transitions
                            .insert(location, CleanupTransition::Exit { edge, state });
                    }
                }
            }
            let mut edge = |target: BlockId| {
                graph.edge_checks += 1;
                propagate(
                    target.0,
                    state,
                    &mut graph.incoming_states,
                    &mut visited,
                    &mut pending,
                )
            };
            match function.blocks[index].terminator {
                Terminator::Jump(target) => edge(target)?,
                Terminator::Branch {
                    then_block,
                    else_block,
                    ..
                } => {
                    edge(then_block)?;
                    edge(else_block)?;
                }
                Terminator::Suspend { resume, .. } => edge(resume)?,
                Terminator::Return(_) | Terminator::CleanupReturn => {}
            }
        }
        if has_scopes && visited.iter().any(|seen| !seen) {
            return Err("unreachable cleanup block");
        }
        graph.order.extend(
            visited
                .iter()
                .enumerate()
                .filter_map(|(index, seen)| (!seen).then_some(index)),
        );
        // Validate all declarations, including blocks omitted from emission.
        // Backend preparation indexes the registration bodies by these sites.
        if site_owners.keys().any(|site| !registrations.contains(site)) {
            return Err("cleanup site has no defer registration instruction");
        }
        if registrations
            .iter()
            .any(|site| !site_owners.contains_key(site))
        {
            return Err("defer registration has no cleanup scope owner");
        }
        graph.regions = regions.len();
        Ok(graph)
    }
}

fn propagate(
    target: usize,
    state: Option<CleanupStateId>,
    entries: &mut [Option<CleanupStateId>],
    visited: &mut [bool],
    pending: &mut Vec<usize>,
) -> Result<(), &'static str> {
    let seen = visited
        .get_mut(target)
        .ok_or("invalid cleanup edge target")?;
    if *seen {
        if entries[target] != state {
            return Err("cleanup scopes disagree across control-flow edges");
        }
    } else {
        *seen = true;
        entries[target] = state;
        pending.push(target);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
