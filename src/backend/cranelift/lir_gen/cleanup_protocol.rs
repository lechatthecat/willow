//! Constant-size identities for diagnostic preparations at cleanup edges.
//!
//! The existing protocol analyses validate complete method/reference metadata.
//! This pass only assigns persistent identities to that already-valid flow;
//! it never copies a preparation stack at a branch or cleanup instruction.
use super::{LirFunction, LirInst, lir_block_successors};
use crate::ir::lowered::{LirOperand, LirRvalue as V};
use std::collections::HashMap;

pub(super) type ProtocolKey = (Option<usize>, Option<usize>);
pub(super) type ProtocolMap = HashMap<(usize, usize), ProtocolKey>;

#[derive(Default)]
struct Nodes {
    parents: Vec<Option<usize>>,
}
impl Nodes {
    fn push(&mut self, parent: Option<usize>) -> Option<usize> {
        let id = self.parents.len();
        self.parents.push(parent);
        Some(id)
    }
    fn pop(&self, current: Option<usize>) -> Option<usize> {
        self.parents[current.expect("validated preparation stack")]
    }
    fn append_record(&mut self, current: Option<usize>) -> Option<usize> {
        // The new node identifies the complete appended record sequence. Its
        // parent remains the enclosing reference scope, not the prior version
        // of this scope, so the eventual call pops the whole scope in O(1).
        self.push(self.pop(current))
    }
}

#[cfg(test)]
#[derive(Default)]
struct Work {
    instructions: usize,
    edges: usize,
    nodes: usize,
}
struct Analysis {
    protocols: ProtocolMap,
    #[cfg(test)]
    work: Work,
}

pub(super) fn cleanup_protocols(function: &LirFunction) -> ProtocolMap {
    analyze(function).protocols
}

impl Analysis {
    fn empty_protocols(function: &LirFunction) -> Self {
        let mut analysis = Analysis {
            protocols: HashMap::new(),
            #[cfg(test)]
            work: Work::default(),
        };
        // Unreachable blocks use the same empty preparation context as the full
        // metadata analyses. A reachable visit below overwrites these defaults.
        for (block, body) in function.blocks.iter().enumerate() {
            for (position, instruction) in body.instrs.iter().enumerate() {
                #[cfg(test)]
                {
                    analysis.work.instructions += 1;
                }
                if matches!(
                    instruction,
                    LirInst::FlushDefers { .. } | LirInst::LeaveDeferScope { .. }
                ) {
                    analysis.protocols.insert((block, position), (None, None));
                }
            }
        }
        analysis
    }
}

fn analyze(function: &LirFunction) -> Analysis {
    let mut analysis = Analysis::empty_protocols(function);
    if function.blocks.is_empty() {
        return analysis;
    }
    let mut methods = Nodes::default();
    let mut references = Nodes::default();
    let mut entries = vec![None; function.blocks.len()];
    entries[0] = Some((None, None));
    let mut pending = vec![0];
    while let Some(block) = pending.pop() {
        let mut current = entries[block].expect("queued protocol entry");
        let mut edges = Vec::new();
        let mut diverged = false;
        for (position, instruction) in function.blocks[block].instrs.iter().enumerate() {
            #[cfg(test)]
            {
                analysis.work.instructions += 1;
            }
            match instruction {
                LirInst::RestoreCallPreparation { entry } => {
                    current = entries[entry.0].expect("dominating loop entry preparation");
                }
                LirInst::FlushDefers { .. } | LirInst::LeaveDeferScope { .. } => {
                    analysis.protocols.insert((block, position), current);
                }
                LirInst::EnterDeferScope {
                    resume: Some(resume),
                    ..
                } => {
                    // A recovered panic replays the preparations that existed
                    // at scope entry, even if its body changed them later.
                    edges.push((resume.0, current));
                }
                LirInst::Compute { value, .. } => {
                    match value {
                        V::PrepareMethod { .. } => current.0 = methods.push(current.0),
                        V::MethodCall { .. } => current.0 = methods.pop(current.0),
                        _ => {}
                    }
                    match value {
                        V::BeginReferenceCall => current.1 = references.push(current.1),
                        V::ReferenceDebug { .. } => current.1 = references.append_record(current.1),
                        V::DirectCall { args, .. }
                        | V::StartTask { args, .. }
                        | V::StaticCall { args, .. }
                        | V::MethodCall { args, .. }
                        | V::ConstructorCall { args, .. }
                            if args
                                .iter()
                                .any(|arg| matches!(arg, LirOperand::Reference { .. })) =>
                        {
                            current.1 = references.pop(current.1);
                        }
                        V::Panic { .. } => {
                            diverged = true;
                            break;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if !diverged {
            edges.extend(
                lir_block_successors(&function.blocks[block])
                    .into_iter()
                    .map(|target| (target, current)),
            );
        }
        for (target, state) in edges {
            #[cfg(test)]
            {
                analysis.work.edges += 1;
            }
            // Full metadata already proved joins equivalent. Distinct source
            // instructions may have made equivalent nodes; retaining the first
            // entry avoids needless revisits without rejecting a valid join.
            if entries[target].is_none() {
                entries[target] = Some(state);
                pending.push(target);
            }
        }
    }
    #[cfg(test)]
    {
        analysis.work.nodes = methods.parents.len() + references.parents.len();
    }
    analysis
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Span;
    use crate::ir::lowered::{BlockId, LirBlock, LirLocalId, LirPlace, Terminator};
    use crate::semantic::ids::{FunctionId, SemanticType as Type};

    fn compute(value: V) -> LirInst {
        LirInst::Compute {
            local: LirLocalId(0),
            value,
            span: Span::new(0, 1, 1, 1),
        }
    }
    fn reference() -> LirOperand {
        LirOperand::Reference {
            place: LirPlace::Local(LirLocalId(0)),
            span: Span::new(0, 1, 1, 1),
            display: "value".into(),
        }
    }
    fn prepare() -> LirInst {
        compute(V::PrepareMethod {
            receiver: LirOperand::Local(LirLocalId(0)),
            receiver_ty: Type::I64,
            method: "run".into(),
        })
    }
    fn record() -> LirInst {
        compute(V::ReferenceDebug {
            argument: reference(),
            callee: FunctionId::free("consume"),
            index: 0,
        })
    }
    fn flush() -> LirInst {
        LirInst::FlushDefers { scopes: 0 }
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

    #[test]
    fn cleanup_protocol_work_is_linear_in_depth_and_exit_fanout() {
        for depth in [1, 8, 64, 512] {
            for exits in [1, 16, 128, 512] {
                let mut preparation = Vec::new();
                for _ in 0..depth {
                    preparation.extend([prepare(), compute(V::BeginReferenceCall), record()]);
                }
                let mut blocks = vec![block(0, preparation, Terminator::Jump(BlockId(1)))];
                for index in 1..=exits {
                    let terminator = if index == exits {
                        Terminator::Jump(BlockId(exits + 1))
                    } else {
                        Terminator::Branch {
                            cond: LirOperand::Bool(true),
                            then_block: BlockId(exits + 1),
                            else_block: BlockId(index + 1),
                        }
                    };
                    blocks.push(block(index, vec![flush()], terminator));
                }
                blocks.push(block(exits + 1, vec![flush()], Terminator::Return(None)));
                let analysis = analyze(&function(blocks));
                assert_eq!(analysis.work.instructions, 2 * (3 * depth + exits + 1));
                assert_eq!(analysis.work.nodes, 3 * depth);
                assert_eq!(analysis.work.edges, 2 * exits);
                assert_eq!(analysis.protocols.len(), exits + 1);
                let expected = analysis.protocols[&(1, 0)];
                for index in 1..=exits + 1 {
                    assert_eq!(analysis.protocols[&(index, 0)], expected);
                }
                println!(
                    "depth={depth} exits={exits} visits={} nodes={} edges={} lookups={}",
                    analysis.work.instructions,
                    analysis.work.nodes,
                    analysis.work.edges,
                    exits + 1
                );
            }
        }
    }

    #[test]
    fn cleanup_protocol_abandoned_suffix_restores_shared_prefix_in_linear_work() {
        for depth in [1, 16, 128] {
            for exits in [1, 16, 128] {
                let mut preparation = Vec::new();
                for _ in 0..depth {
                    preparation.extend([prepare(), compute(V::BeginReferenceCall), record()]);
                }
                let mut blocks = vec![block(0, preparation, Terminator::Jump(BlockId(1)))];
                let end = 2 * exits + 1;
                for index in 1..=exits {
                    let target = BlockId(exits + index);
                    let terminator = if index == exits {
                        Terminator::Jump(target)
                    } else {
                        Terminator::Branch {
                            cond: LirOperand::Bool(true),
                            then_block: target,
                            else_block: BlockId(index + 1),
                        }
                    };
                    blocks.push(block(index, vec![], terminator));
                }
                for index in 1..=exits {
                    blocks.push(block(
                        exits + index,
                        vec![
                            prepare(),
                            compute(V::BeginReferenceCall),
                            record(),
                            LirInst::RestoreCallPreparation { entry: BlockId(1) },
                            flush(),
                        ],
                        Terminator::Jump(BlockId(end)),
                    ));
                }
                blocks.push(block(end, vec![flush()], Terminator::Return(None)));
                let analysis = analyze(&function(blocks));
                assert_eq!(analysis.work.instructions, 2 * (3 * depth + 5 * exits + 1));
                assert_eq!(analysis.work.nodes, 3 * (depth + exits));
                assert_eq!(analysis.work.edges, 3 * exits);
                for index in 1..=exits {
                    assert_eq!(
                        analysis.protocols[&(exits + index, 4)],
                        analysis.protocols[&(end, 0)]
                    );
                }
                println!(
                    "restore depth={depth} exits={exits} visits={} nodes={} edges={}",
                    analysis.work.instructions, analysis.work.nodes, analysis.work.edges
                );
            }
        }
    }

    #[test]
    fn cleanup_protocol_reference_updates_pop_to_the_enclosing_scope() {
        let graph = function(vec![block(
            0,
            vec![
                prepare(),
                compute(V::BeginReferenceCall),
                record(),
                flush(),
                prepare(),
                compute(V::BeginReferenceCall),
                record(),
                record(),
                flush(),
                compute(V::MethodCall {
                    receiver: LirOperand::Local(LirLocalId(0)),
                    receiver_ty: Type::I64,
                    method: "run".into(),
                    is_async: false,
                    args: vec![reference()],
                    arg_types: vec![Type::I64],
                    result: Type::Void,
                    super_class: None,
                }),
                flush(),
            ],
            Terminator::Return(None),
        )]);
        let keys = cleanup_protocols(&graph);
        assert_ne!(keys[&(0, 3)], keys[&(0, 8)]);
        assert_eq!(keys[&(0, 3)], keys[&(0, 10)]);
    }

    #[test]
    fn cleanup_protocol_recovery_keeps_entry_state_and_panic_stops_normal_flow() {
        let graph = function(vec![
            block(
                0,
                vec![
                    prepare(),
                    compute(V::BeginReferenceCall),
                    record(),
                    flush(),
                    LirInst::EnterDeferScope {
                        sites: vec![],
                        resume: Some(BlockId(2)),
                        lock: None,
                    },
                ],
                Terminator::Jump(BlockId(1)),
            ),
            block(
                1,
                vec![
                    prepare(),
                    compute(V::BeginReferenceCall),
                    record(),
                    compute(V::Panic {
                        message: LirOperand::Int(0),
                    }),
                ],
                Terminator::Jump(BlockId(2)),
            ),
            block(2, vec![flush()], Terminator::Return(None)),
            block(3, vec![flush()], Terminator::Return(None)),
        ]);
        let keys = cleanup_protocols(&graph);
        assert_eq!(keys[&(0, 3)], keys[&(2, 0)]);
        assert_eq!(keys[&(3, 0)], (None, None));
    }
}
