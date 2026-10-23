use super::*;
use crate::diagnostics::Span;
use crate::ir::lowered::{LirBlock, LirOperand};

fn enter(site: u32, resume: Option<usize>) -> LirInst {
    LirInst::EnterDeferScope {
        sites: vec![(LirDeferId(site), Span::new(0, 0, 1, 1))],
        resume: resume.map(BlockId),
        lock: None,
    }
}
fn leave(site: u32) -> LirInst {
    LirInst::LeaveDeferScope {
        sites: vec![LirDeferId(site)],
    }
}
fn block(id: usize, instrs: Vec<LirInst>, terminator: Terminator) -> LirBlock {
    LirBlock {
        id: BlockId(id),
        instrs,
        terminator,
        recovery: vec![],
    }
}
fn function(blocks: Vec<LirBlock>) -> LirFunction {
    let mut function = LirFunction::empty_artifact_region();
    function.blocks = blocks;
    function
}
fn branch(left: usize, right: usize) -> Terminator {
    Terminator::Branch {
        cond: LirOperand::Bool(true),
        then_block: BlockId(left),
        else_block: BlockId(right),
    }
}

#[test]
fn cleanup_equivalent_block_permutation_preserves_scope_transfers() {
    for (body, exit) in [(1, 2), (2, 1)] {
        let mut blocks = vec![
            block(
                0,
                vec![enter(0, None), register(0)],
                Terminator::Jump(BlockId(body)),
            ),
            block(1, vec![], Terminator::Return(None)),
            block(2, vec![], Terminator::Return(None)),
        ];
        blocks[body] = block(body, vec![leave(0)], Terminator::Jump(BlockId(exit)));
        let graph = CleanupGraph::lower(&function(blocks)).unwrap();
        assert_eq!(graph.entries[body], Some(RootScopeId(0)));
        assert_eq!(graph.entries[exit], None);
        assert_eq!(graph.exits[&(body, 0)].from, Some(RootScopeId(0)));
        assert_eq!(graph.exits[&(body, 0)].to, None);
        assert_eq!(graph.regions, 1);
    }
}

#[test]
fn cleanup_recovery_and_normal_join_must_agree() {
    let bad = function(vec![
        block(0, vec![enter(0, Some(1))], Terminator::Jump(BlockId(1))),
        block(1, vec![], Terminator::Return(None)),
    ]);
    assert_eq!(
        CleanupGraph::lower(&bad).unwrap_err(),
        "cleanup scopes disagree across control-flow edges"
    );
    let good = function(vec![
        block(
            0,
            vec![enter(0, Some(1)), register(0), leave(0)],
            Terminator::Jump(BlockId(1)),
        ),
        block(1, vec![], Terminator::Return(None)),
    ]);
    assert!(CleanupGraph::lower(&good).is_ok());
}

#[test]
fn cleanup_rejects_scope_underflow() {
    let graph = function(vec![block(0, vec![leave(0)], Terminator::Return(None))]);
    assert_eq!(
        CleanupGraph::lower(&graph).unwrap_err(),
        "cleanup scope underflow"
    );
}

#[test]
fn cleanup_rejects_invalid_normal_and_recovery_targets() {
    for graph in [
        function(vec![block(0, vec![], Terminator::Jump(BlockId(1)))]),
        function(vec![block(
            0,
            vec![enter(0, Some(1))],
            Terminator::Return(None),
        )]),
        function(vec![block(0, vec![], branch(0, 1))]),
    ] {
        assert_eq!(
            CleanupGraph::lower(&graph).unwrap_err(),
            "invalid cleanup edge target"
        );
    }
}

#[test]
fn cleanup_nested_flush_preserves_outer_scope_and_panic_parent() {
    let graph = CleanupGraph::lower(&function(vec![block(
        0,
        vec![
            enter(0, None),
            register(0),
            enter(1, None),
            register(1),
            enter(2, None),
            register(2),
            LirInst::FlushDefers {
                sites: vec![LirDeferId(2), LirDeferId(1)],
            },
            leave(0),
        ],
        Terminator::Return(None),
    )]))
    .unwrap();
    assert_eq!(graph.scopes[0].panic, PanicTarget::Propagate);
    assert_eq!(graph.scopes[2].parent, Some(RootScopeId(1)));
    assert_eq!(graph.scopes[2].panic, PanicTarget::Scope(RootScopeId(1)));
    let flush = graph.exits[&(0, 6)];
    assert_eq!(
        (flush.from, flush.to, flush.scopes),
        (Some(RootScopeId(2)), Some(RootScopeId(0)), 2)
    );
    assert_eq!(graph.exits[&(0, 7)].to, None);
    assert_eq!(graph.regions, 2);
}

#[test]
fn cleanup_flush_rejects_non_suffix_unknown_and_incomplete_sites() {
    for sites in [vec![0], vec![9], vec![1], vec![1, 2, 9]] {
        let mut inner = enter(1, None);
        if let LirInst::EnterDeferScope { sites, .. } = &mut inner {
            sites.push((LirDeferId(2), Span::new(0, 0, 1, 1)));
        }
        let graph = function(vec![block(
            0,
            vec![
                enter(0, None),
                inner,
                LirInst::FlushDefers {
                    sites: sites.into_iter().map(LirDeferId).collect(),
                },
            ],
            Terminator::Return(None),
        )]);
        assert_eq!(
            CleanupGraph::lower(&graph).unwrap_err(),
            "cleanup flush sites are not a complete active scope suffix"
        );
    }
    let no_owner = function(vec![block(
        0,
        vec![LirInst::FlushDefers {
            sites: vec![LirDeferId(0)],
        }],
        Terminator::Return(None),
    )]);
    assert!(CleanupGraph::lower(&no_owner).is_err());
}

#[test]
fn cleanup_leave_rejects_wrong_or_incomplete_sites() {
    for sites in [
        vec![],
        vec![LirDeferId(1)],
        vec![LirDeferId(0), LirDeferId(1)],
    ] {
        let graph = function(vec![block(
            0,
            vec![enter(0, None), LirInst::LeaveDeferScope { sites }],
            Terminator::Return(None),
        )]);
        assert_eq!(
            CleanupGraph::lower(&graph).unwrap_err(),
            "cleanup leave sites disagree with current scope"
        );
    }
}

#[test]
fn cleanup_defer_requires_current_scope_ownership() {
    for site in [0, 1, 2] {
        let mut graph = function(vec![block(
            0,
            vec![
                enter(0, None),
                enter(1, None),
                LirInst::Defer {
                    id: LirDeferId(site),
                    body: crate::ir::lowered::LirDeferBody {
                        function: Box::new(LirFunction::empty_artifact_region()),
                        captures: vec![],
                        recovery_capable: false,
                    },
                    span: Span::new(0, 0, 1, 1),
                },
            ],
            Terminator::Return(None),
        )]);
        if site == 1 {
            graph.blocks[0].instrs.insert(1, register(0));
            assert!(CleanupGraph::lower(&graph).is_ok());
        } else {
            assert_eq!(
                CleanupGraph::lower(&graph).unwrap_err(),
                "defer site is not owned by current cleanup scope"
            );
        }
    }
}

#[test]
fn cleanup_deep_chain_has_one_node_and_transition_per_scope() {
    for count in [8, 64, 512] {
        let mut instructions: Vec<_> = (0..count)
            .flat_map(|id| [enter(id, None), register(id)])
            .collect();
        instructions.extend((0..count).rev().map(leave));
        let graph = CleanupGraph::lower(&function(vec![block(
            0,
            instructions,
            Terminator::Return(None),
        )]))
        .unwrap();
        println!(
            "cleanup deep_chain input={count} blocks={} scopes={} enters={} exits={} regions={}",
            graph.entries.len(),
            graph.scopes.len(),
            graph.enters.len(),
            graph.exits.len(),
            graph.regions
        );
        assert_eq!(graph.scopes.len(), count as usize);
        assert_eq!(graph.enters.len(), count as usize);
        assert_eq!(graph.exits.len(), count as usize);
        assert_eq!(graph.regions, count as usize);
        assert_eq!(
            graph.exits.values().map(|edge| edge.scopes).sum::<usize>(),
            count as usize
        );
    }
}

#[test]
fn cleanup_fanout_reuses_one_region_for_equivalent_exits() {
    for leaves in [4, 32, 256] {
        // A binary decision spine reaches N exits, all carrying one scope ID.
        let mut blocks = vec![block(
            0,
            vec![enter(0, None), register(0)],
            Terminator::Jump(BlockId(1)),
        )];
        for index in 1..leaves {
            let next = if index + 1 == leaves {
                2 * leaves - 1
            } else {
                index + 1
            };
            blocks.push(block(index, vec![], branch(leaves + index - 1, next)));
        }
        for index in leaves..2 * leaves {
            blocks.push(block(index, vec![leave(0)], Terminator::Return(None)));
        }
        let graph = CleanupGraph::lower(&function(blocks)).unwrap();
        println!(
            "cleanup fanout input={leaves} blocks={} scopes={} enters={} exits={} regions={}",
            graph.entries.len(),
            graph.scopes.len(),
            graph.enters.len(),
            graph.exits.len(),
            graph.regions
        );
        assert_eq!(graph.scopes.len(), 1);
        assert_eq!(graph.entries.len(), 2 * leaves);
        assert_eq!(graph.exits.len(), leaves);
        assert_eq!(graph.regions, 1);
        assert!(
            graph
                .exits
                .values()
                .all(|edge| edge.region == CleanupRegionId(0))
        );
    }
}

#[test]
fn cleanup_leave_matches_site_sets_independent_of_order_and_duplicates() {
    let mut scope = enter(0, None);
    if let LirInst::EnterDeferScope { sites, .. } = &mut scope {
        sites.push((LirDeferId(1), Span::new(0, 0, 1, 1)));
    }
    let graph = function(vec![block(
        0,
        vec![
            scope,
            register(0),
            register(1),
            LirInst::LeaveDeferScope {
                sites: vec![LirDeferId(1), LirDeferId(0), LirDeferId(1)],
            },
        ],
        Terminator::Return(None),
    )]);
    assert!(CleanupGraph::lower(&graph).is_ok());
}

#[test]
fn cleanup_flush_accepts_complete_suffix_with_duplicate_requests() {
    let graph = function(vec![block(
        0,
        vec![
            enter(0, None),
            register(0),
            enter(1, None),
            register(1),
            LirInst::FlushDefers {
                sites: vec![LirDeferId(1), LirDeferId(1)],
            },
            leave(0),
        ],
        Terminator::Return(None),
    )]);
    let lowered = CleanupGraph::lower(&graph).unwrap();
    assert_eq!(lowered.exits[&(0, 4)].scopes, 1);
}

#[test]
fn cleanup_rejects_site_owned_by_distinct_scopes() {
    let graph = function(vec![block(
        0,
        vec![enter(0, None), enter(0, None)],
        Terminator::Return(None),
    )]);
    assert_eq!(
        CleanupGraph::lower(&graph).unwrap_err(),
        "defer site is owned by multiple cleanup scopes"
    );
}

#[test]
fn cleanup_rejects_duplicate_registration_instructions() {
    let registration = LirInst::Defer {
        id: LirDeferId(0),
        body: crate::ir::lowered::LirDeferBody {
            function: Box::new(LirFunction::empty_artifact_region()),
            captures: vec![],
            recovery_capable: false,
        },
        span: Span::new(0, 0, 1, 1),
    };
    let graph = function(vec![block(
        0,
        vec![enter(0, None), registration.clone(), registration],
        Terminator::Return(None),
    )]);
    assert_eq!(
        CleanupGraph::lower(&graph).unwrap_err(),
        "duplicate defer registration instruction"
    );
}

fn register(site: u32) -> LirInst {
    LirInst::Defer {
        id: LirDeferId(site),
        body: crate::ir::lowered::LirDeferBody {
            function: Box::new(LirFunction::empty_artifact_region()),
            captures: vec![],
            recovery_capable: false,
        },
        span: Span::new(0, 0, 1, 1),
    }
}

#[test]
fn cleanup_rejects_registration_prefix_divergence_at_join() {
    let graph = function(vec![
        block(0, vec![enter(0, None)], branch(1, 2)),
        block(1, vec![register(0)], Terminator::Jump(BlockId(3))),
        block(2, vec![], Terminator::Jump(BlockId(3))),
        block(3, vec![leave(0)], Terminator::Return(None)),
    ]);
    assert_eq!(
        CleanupGraph::lower(&graph).unwrap_err(),
        "cleanup scopes disagree across control-flow edges"
    );
}

#[test]
fn cleanup_registration_prefix_survives_nested_scope_and_recovery() {
    let graph = CleanupGraph::lower(&function(vec![
        block(
            0,
            vec![enter(0, None), register(0), enter(1, Some(2)), register(1)],
            Terminator::Jump(BlockId(1)),
        ),
        block(1, vec![leave(1)], Terminator::Jump(BlockId(2))),
        block(2, vec![leave(0)], Terminator::Return(None)),
    ]))
    .unwrap();
    let outer = graph.after[&(0, 1)];
    assert_eq!(graph.incoming_states[2], outer);
    assert_eq!(graph.scopes[1].parent_state, outer);
    assert_eq!(graph.before[&(1, 0)], graph.after[&(0, 3)]);
    assert_eq!(graph.after[&(1, 0)], outer);
    assert_eq!(graph.states.len(), 4);
    assert_eq!(graph.registrations.len(), 2);
    assert_eq!(graph.registrations[0].previous, None);
    assert_eq!(graph.registrations[1].previous, None);
}

#[test]
fn cleanup_registration_list_is_persistent_and_source_neutral() {
    let mut scope = enter(0, None);
    if let LirInst::EnterDeferScope { sites, .. } = &mut scope {
        sites.push((LirDeferId(1), Span::new(0, 0, 1, 1)));
    }
    let graph = CleanupGraph::lower(&function(vec![
        block(0, vec![scope, register(0)], Terminator::Jump(BlockId(1))),
        block(
            1,
            vec![
                register(1),
                LirInst::LeaveDeferScope {
                    sites: vec![LirDeferId(0), LirDeferId(1)],
                },
            ],
            Terminator::Return(None),
        ),
    ]))
    .unwrap();
    assert_eq!(graph.states.len(), 3);
    assert_eq!(graph.registrations.len(), 2);
    assert_eq!(graph.registrations[1].previous, Some(RegistrationId(0)));
    assert_eq!(
        graph.states[graph.before[&(1, 0)].unwrap().0].registrations,
        Some(RegistrationId(0))
    );
    assert_eq!(
        graph.states[graph.after[&(1, 0)].unwrap().0].registrations,
        Some(RegistrationId(1))
    );
    assert_eq!(graph.before.len(), 4);
    assert_eq!(graph.transitions.len(), 4);
}

#[test]
fn cleanup_deep_scope_fanout_shares_states_and_registration_nodes() {
    for (depth, leaves) in [(8, 8), (64, 64), (256, 256)] {
        let mut instructions = Vec::new();
        for id in 0..depth {
            instructions.push(enter(id as u32, None));
            instructions.push(register(id as u32));
        }
        let mut blocks = vec![block(0, instructions, Terminator::Jump(BlockId(1)))];
        for index in 1..leaves {
            let next = if index + 1 == leaves {
                2 * leaves - 1
            } else {
                index + 1
            };
            blocks.push(block(index, vec![], branch(leaves + index - 1, next)));
        }
        for index in leaves..2 * leaves {
            blocks.push(block(index, vec![], Terminator::Return(None)));
        }
        let graph = CleanupGraph::lower(&function(blocks)).unwrap();
        assert_eq!(graph.states.len(), 2 * depth);
        assert_eq!(graph.registrations.len(), depth);
        assert_eq!(graph.edge_checks, 2 * leaves - 1);
        assert_eq!(graph.order.len(), 2 * leaves);
        assert!(
            graph.incoming_states[1..]
                .iter()
                .all(|state| *state == graph.after[&(0, 2 * depth - 1)])
        );
        println!(
            "cleanup persistent depth={depth} fanout={leaves} states={} registrations={} edge_checks={}",
            graph.states.len(),
            graph.registrations.len(),
            graph.edge_checks
        );
    }
}

#[test]
fn cleanup_panic_program_has_explicit_lifo_consumption_and_dispatch() {
    for lock in [false, true] {
        let scope = RootScopeId(3);
        let program = PanicProgram::lower(
            scope,
            &[LirDeferId(0), LirDeferId(1)],
            lock,
            Some(BlockId(7)),
            PanicTarget::Scope(RootScopeId(2)),
        );
        assert_eq!(program.blocks.len(), 5);
        for (check, site) in [(0, LirDeferId(1)), (2, LirDeferId(0))] {
            assert!(program.blocks[check].instructions.is_empty());
            assert_eq!(
                program.blocks[check].terminator,
                PanicTerminator::IfRegistered {
                    site,
                    run: check + 1,
                    next: check + 2
                }
            );
            assert_eq!(
                program.blocks[check + 1].instructions,
                vec![
                    PanicInstruction::Consume(site),
                    PanicInstruction::Invoke(site)
                ]
            );
            assert_eq!(
                program.blocks[check + 1].terminator,
                PanicTerminator::Jump(check + 2)
            );
        }
        let expected = if lock {
            vec![
                PanicInstruction::ReleaseLock(scope),
                PanicInstruction::RestoreRoots(scope),
            ]
        } else {
            vec![PanicInstruction::RestoreRoots(scope)]
        };
        assert_eq!(program.blocks[4].instructions, expected);
        assert_eq!(
            program.blocks[4].terminator,
            PanicTerminator::Dispatch {
                scope,
                recovery: Some(BlockId(7)),
                propagate: PanicTarget::Scope(RootScopeId(2))
            }
        );
    }
}

#[test]
fn cleanup_panic_program_empty_scope_restores_roots_and_propagates() {
    let program = PanicProgram::lower(RootScopeId(0), &[], false, None, PanicTarget::Propagate);
    assert_eq!(program.blocks.len(), 1);
    assert_eq!(
        program.blocks[0].instructions,
        vec![PanicInstruction::RestoreRoots(RootScopeId(0))]
    );
    assert_eq!(
        program.blocks[0].terminator,
        PanicTerminator::Dispatch {
            scope: RootScopeId(0),
            recovery: None,
            propagate: PanicTarget::Propagate
        }
    );
}

#[test]
fn cleanup_panic_program_deduplicates_sites_and_scales_linearly() {
    for count in [8, 64, 512] {
        let sites = (0..count)
            .flat_map(|site| [(LirDeferId(site), Span::new(0, 0, 1, 1)); 2])
            .collect();
        let mut instructions = vec![LirInst::EnterDeferScope {
            sites,
            resume: None,
            lock: None,
        }];
        instructions.extend((0..count).map(register));
        let graph = CleanupGraph::lower(&function(vec![block(
            0,
            instructions,
            Terminator::Return(None),
        )]))
        .unwrap();
        let scope = &graph.scopes[0];
        assert_eq!(scope.sites.len(), count as usize);
        assert_eq!(scope.panic_program.blocks.len(), 2 * count as usize + 1);
        assert_eq!(
            scope
                .panic_program
                .blocks
                .iter()
                .map(|block| block.instructions.len())
                .sum::<usize>(),
            2 * count as usize + 1
        );
        assert_eq!(scope.sites, (0..count).map(LirDeferId).collect::<Vec<_>>());
        println!(
            "cleanup panic sites={count} blocks={} instructions={}",
            scope.panic_program.blocks.len(),
            scope
                .panic_program
                .blocks
                .iter()
                .map(|block| block.instructions.len())
                .sum::<usize>()
        );
    }
}

#[test]
fn cleanup_rejects_declared_site_without_registration_body() {
    let graph = function(vec![block(
        0,
        vec![enter(0, None), leave(0)],
        Terminator::Return(None),
    )]);
    assert_eq!(
        CleanupGraph::lower(&graph).unwrap_err(),
        "cleanup site has no defer registration instruction"
    );
}

#[test]
fn cleanup_rejects_unowned_registration_in_unreachable_block() {
    let graph = function(vec![
        block(0, vec![], Terminator::Return(None)),
        block(1, vec![register(0)], Terminator::Return(None)),
    ]);
    assert_eq!(
        CleanupGraph::lower(&graph).unwrap_err(),
        "defer registration has no cleanup scope owner"
    );
}

#[test]
fn cleanup_rejects_registration_without_any_scope() {
    let graph = function(vec![block(0, vec![register(0)], Terminator::Return(None))]);
    assert_eq!(
        CleanupGraph::lower(&graph).unwrap_err(),
        "defer outside a cleanup scope"
    );
}
