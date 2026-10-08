//! Conservative native root-slot coloring (willow-jz15.50).
//!
//! Only single-definition locals whose entire storage lifetime is in one LIR
//! block participate. Clears are writes and extend that lifetime; address uses,
//! captures, terminators, repeated definitions and cross-block accesses reject
//! a local. Thus loops, recovery edges and arbitrary block emission order need
//! no global liveness fixed point. Poll functions do not use this analysis.
//!
//! Two linear scans record intervals then bucket their endpoints by instruction.
//! Free lists are keyed by width; no pairwise interference graph or sorting.
//! Cost is O(blocks + locals + instructions + operand occurrences), with expected O(1)
//! width lookup, and O(blocks + locals + largest block) auxiliary space.

use super::transient_roots::{inst_locals, terminator_locals};
use crate::ir::lowered::{LirFunction, LirInst, LirLocalId, LirOperand};
use std::collections::HashMap;

#[derive(Clone, Copy)]
enum Lifetime {
    Unseen,
    Local {
        block: usize,
        first: usize,
        last: usize,
    },
    Rejected,
}

/// Each returned group is one physical slot. `widths` selects eligible rooted
/// locals (None excludes parameters, existing storage and non-GC values).
pub(super) fn groups(f: &LirFunction, widths: &[Option<u32>]) -> Vec<Vec<LirLocalId>> {
    let mut states: Vec<_> = widths
        .iter()
        .map(|w| {
            if w.is_some() {
                Lifetime::Unseen
            } else {
                Lifetime::Rejected
            }
        })
        .collect();
    let touch = |states: &mut [Lifetime], id: LirLocalId, block: usize, at: usize| {
        states[id.0 as usize] = match states[id.0 as usize] {
            Lifetime::Local {
                block: owner,
                first,
                ..
            } if owner == block => Lifetime::Local {
                block,
                first,
                last: at,
            },
            _ => Lifetime::Rejected,
        };
    };
    for (block_index, block) in f.blocks.iter().enumerate() {
        for (at, inst) in block.instrs.iter().enumerate() {
            let definition = match inst {
                LirInst::Compute { local, value, .. } => {
                    for operand in value.operands() {
                        for id in operand.locals() {
                            if matches!(operand, LirOperand::Reference { .. }) {
                                states[id.0 as usize] = Lifetime::Rejected;
                            } else {
                                touch(&mut states, id, block_index, at);
                            }
                        }
                    }
                    Some(*local)
                }
                LirInst::Let { local, value, .. } => {
                    for id in value.locals() {
                        if matches!(value, LirOperand::Reference { .. }) {
                            states[id.0 as usize] = Lifetime::Rejected;
                        } else {
                            touch(&mut states, id, block_index, at);
                        }
                    }
                    Some(*local)
                }
                LirInst::ClearScopeRoots { locals } => {
                    for &id in locals {
                        touch(&mut states, id, block_index, at);
                    }
                    None
                }
                _ => {
                    // Includes mutable assignments, match bindings, select,
                    // locks and defer captures. Keep their original storage.
                    for id in inst_locals(inst) {
                        states[id.0 as usize] = Lifetime::Rejected;
                    }
                    None
                }
            };
            if let Some(id) = definition {
                states[id.0 as usize] = match states[id.0 as usize] {
                    Lifetime::Unseen => Lifetime::Local {
                        block: block_index,
                        first: at,
                        last: at,
                    },
                    _ => Lifetime::Rejected,
                };
            }
        }
        for id in terminator_locals(&block.terminator) {
            states[id.0 as usize] = Lifetime::Rejected;
        }
    }
    let mut by_block = vec![Vec::new(); f.blocks.len()];
    for (id, state) in states.into_iter().enumerate() {
        if let Lifetime::Local { block, first, last } = state {
            by_block[block].push((LirLocalId(id as u32), first, last));
        }
    }
    let mut groups: Vec<Vec<LirLocalId>> = Vec::new();
    let mut free: HashMap<u32, Vec<usize>> = HashMap::new();
    for (block, intervals) in f.blocks.iter().zip(by_block) {
        if intervals.is_empty() {
            continue;
        }
        let mut starts = vec![None; block.instrs.len()];
        let mut ends = vec![Vec::new(); block.instrs.len()];
        for (id, first, last) in intervals {
            starts[first] = Some((id, last));
        }
        for (at, start) in starts.into_iter().enumerate() {
            if let Some((id, last)) = start {
                let width = widths[id.0 as usize].unwrap();
                let group = free.entry(width).or_default().pop().unwrap_or_else(|| {
                    groups.push(Vec::new());
                    groups.len() - 1
                });
                groups[group].push(id);
                ends[last].push((width, group));
            }
            // Release AFTER the instruction: a definition cannot clobber its
            // own operand, nor a clear erase another local's new value.
            for (width, group) in ends[at].drain(..) {
                free.entry(width).or_default().push(group);
            }
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Span;
    use crate::ir::lowered::{
        BlockId, LirBlock, LirLocal, LirPlace, LirRvalue, LirStorageKind, Terminator,
    };
    use crate::semantic::ids::SemanticType as Type;

    fn define(id: u32, operand: LirOperand) -> LirInst {
        LirInst::Compute {
            local: LirLocalId(id),
            value: LirRvalue::Use(operand),
            span: Span::dummy(),
        }
    }
    fn clear(ids: &[u32]) -> LirInst {
        LirInst::ClearScopeRoots {
            locals: ids.iter().copied().map(LirLocalId).collect(),
        }
    }
    fn function(count: usize, blocks: Vec<Vec<LirInst>>) -> LirFunction {
        let mut f = LirFunction::empty_artifact_region();
        f.locals = (0..count)
            .map(|id| LirLocal {
                id: LirLocalId(id as u32),
                name: format!("v{id}"),
                ty: Type::String,
                storage_kind: LirStorageKind::Value,
                source_span: None,
                synthetic: true,
                parameter: false,
            })
            .collect();
        f.blocks = blocks
            .into_iter()
            .enumerate()
            .map(|(id, instrs)| LirBlock {
                id: BlockId(id),
                instrs,
                terminator: Terminator::Return(None),
                recovery: Vec::new(),
            })
            .collect();
        f
    }
    fn plan(f: &LirFunction) -> Vec<Vec<LirLocalId>> {
        groups(f, &vec![Some(8); f.locals.len()])
    }

    #[test]
    fn stack_slots_disjoint_and_fanout_scale_with_peak_live_storage() {
        for count in [1, 8, 64, 512, 4096] {
            let disjoint = function(
                count,
                vec![
                    (0..count as u32)
                        .flat_map(|i| [define(i, LirOperand::Int(0)), clear(&[i])])
                        .collect(),
                ],
            );
            let fanout = function(
                count,
                (0..count as u32)
                    .map(|i| vec![define(i, LirOperand::Int(0)), clear(&[i])])
                    .collect(),
            );
            let overlapping = function(
                count,
                vec![
                    (0..count as u32)
                        .map(|i| define(i, LirOperand::Int(0)))
                        .chain([clear(&(0..count as u32).collect::<Vec<_>>())])
                        .collect(),
                ],
            );
            assert_eq!(plan(&disjoint).len(), 1);
            assert_eq!(plan(&fanout).len(), 1);
            assert_eq!(plan(&overlapping).len(), count);
            println!("locals={count} sequential_slots=1 fanout_slots=1 simultaneous_slots={count}");
        }
    }

    #[test]
    fn stack_slots_clears_reads_widths_and_exclusions() {
        let f = function(
            3,
            vec![vec![
                define(0, LirOperand::Int(0)),
                define(1, LirOperand::Local(LirLocalId(0))),
                clear(&[0, 1]),
                define(2, LirOperand::Int(0)),
            ]],
        );
        assert_eq!(plan(&f).len(), 2, "operand and output must not alias");
        assert_eq!(groups(&f, &[Some(8), Some(16), Some(8)]).len(), 2);
        assert_eq!(
            groups(&f, &[None, None, Some(8)]),
            vec![vec![LirLocalId(2)]]
        );
        let f = function(
            2,
            vec![vec![
                define(0, LirOperand::Int(0)),
                define(1, LirOperand::Int(0)),
                clear(&[0]),
            ]],
        );
        assert_eq!(plan(&f).len(), 2, "late clear cannot erase a replacement");
        let disjoint_widths = function(
            3,
            vec![vec![
                define(0, LirOperand::Int(0)),
                define(1, LirOperand::Int(0)),
                define(2, LirOperand::Int(0)),
            ]],
        );
        assert_eq!(
            groups(&disjoint_widths, &[Some(8), Some(16), Some(8)]),
            vec![vec![LirLocalId(0), LirLocalId(2)], vec![LirLocalId(1)]],
            "disjoint values reuse only equal widths"
        );
        let f = function(1, vec![vec![clear(&[0]), define(0, LirOperand::Int(0))]]);
        assert!(plan(&f).is_empty(), "use before definition");
        let f = function(
            1,
            vec![vec![
                define(0, LirOperand::Int(0)),
                define(0, LirOperand::Int(1)),
            ]],
        );
        assert!(plan(&f).is_empty(), "multiple definitions");
        let f = function(
            1,
            vec![vec![define(0, LirOperand::Int(0))], vec![clear(&[0])]],
        );
        assert!(plan(&f).is_empty(), "cross-block clear");
        let mut f = function(1, vec![vec![define(0, LirOperand::Int(0))]]);
        f.blocks[0].terminator = Terminator::Return(Some(LirOperand::Local(LirLocalId(0))));
        assert!(plan(&f).is_empty(), "terminator live-out");
        let f = function(
            2,
            vec![vec![
                define(0, LirOperand::Int(0)),
                define(
                    1,
                    LirOperand::Reference {
                        place: LirPlace::Local(LirLocalId(0)),
                        span: Span::dummy(),
                        display: "v0".into(),
                    },
                ),
            ]],
        );
        assert_eq!(
            plan(&f),
            vec![vec![LirLocalId(1)]],
            "address-taken storage excluded"
        );
    }
}
