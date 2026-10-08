//! Machine-code block placement policy (willow-nzsg).
//!
//! Cranelift lowers blocks in the reverse postorder of a successor DFS and
//! sinks only blocks marked cold (or ending in a trap) to the end of the
//! function. Two code-generation facts therefore decide what sits inside a
//! hot loop:
//!
//! * A slow path marked cold usually continues through blocks that its
//!   runtime call creates (cancellation and panic-depth checks). Those are not
//!   cold by themselves, so they stay in the middle of the hot code, and the
//!   fast path jumps over them. [`sink_cold_regions`] marks every block that
//!   is reachable only through cold blocks as cold.
//! * The DFS places a two-way branch's *second* successor immediately after
//!   it. [`lir_loop_depths`] lets the LIR emitter orient a branch so the
//!   successor that stays inside more call-free loops is the fall-through,
//!   placing such a loop's body after its header and its exit after the loop.
//!
//! Both only change placement; no instruction executes differently.
//!
//! Orientation is limited to loops that call no compiled function
//! (willow-3za7). It also changes the dominator-tree visiting order of
//! Cranelift's egraph elaboration: once a loop's body is visited before its
//! exit, Cranelift hoists argument-free values such as the `func_addr` targets
//! of guarded interface dispatch out of the loop. Across the loop's calls each
//! one then holds a callee-saved register, and the loop's own state spills to
//! the stack (virtual_dispatch: 57 -> 67 ms). A loop that calls keeps the
//! source order, whose exit-first visit leaves those values at their uses.
//! Runtime helpers behind inline fast paths, such as an array push, do not
//! count.

use crate::ir::lowered::{LirFunction, LirInst, LirRvalue, Terminator};
use cranelift_codegen::entity::EntitySet;
use cranelift_codegen::ir::Function;

/// Mark every block that is not reachable from the entry without passing
/// through an (effectively) cold block as cold. O(blocks + edges).
pub(super) fn sink_cold_regions(func: &mut Function) {
    let Some(entry) = func.layout.entry_block() else {
        return;
    };
    let mut hot = EntitySet::new();
    let mut pending = vec![entry];
    hot.insert(entry);
    while let Some(block) = pending.pop() {
        for succ in func.block_successors(block) {
            if !func.is_effectively_cold(succ) && hot.insert(succ) {
                pending.push(succ);
            }
        }
    }
    let cold: Vec<_> = func
        .layout
        .blocks()
        .filter(|&block| !hot.contains(block))
        .collect();
    for block in cold {
        func.layout.set_cold(block);
    }
    #[cfg(test)]
    CAPTURE.with(|capture| {
        if let Some(functions) = capture.borrow_mut().as_mut() {
            functions.push(func.clone());
        }
    });
}

#[cfg(test)]
thread_local! {
    /// Final CLIF of every function placed while a test capture is active.
    static CAPTURE: std::cell::RefCell<Option<Vec<Function>>> = const { std::cell::RefCell::new(None) };
}

/// Loop nesting of each LIR block.
pub(super) struct LoopDepths {
    /// The number of loops containing the block.
    pub(super) all: Vec<u32>,
    /// The number of those loops whose every block ends in a jump or branch
    /// and calls no compiled function ([`calls_compiled_code`]).
    pub(super) call_free: Vec<u32>,
}

/// Whether `inst` calls a compiled Willow function or method. Intrinsics and
/// runtime helpers do not count: an array push or index keeps its runtime call
/// on a cold slow path, and their callees are not dispatch targets.
fn calls_compiled_code(inst: &LirInst) -> bool {
    matches!(
        inst,
        LirInst::Compute {
            value: LirRvalue::DirectCall { .. }
                | LirRvalue::IndirectCall { .. }
                | LirRvalue::StaticCall { .. }
                | LirRvalue::MethodCall { .. }
                | LirRvalue::ConstructorCall { .. },
            ..
        }
    )
}

/// The DFS-backedge loops containing each LIR block. A loop is the set of
/// blocks that reach a backedge source without passing its header, so the
/// work is O((blocks + edges) * loop nesting depth + instructions).
pub(super) fn lir_loop_depths(f: &LirFunction) -> LoopDepths {
    let n = f.blocks.len();
    let block_call_free: Vec<bool> = f
        .blocks
        .iter()
        .map(|block| {
            matches!(
                block.terminator,
                Terminator::Jump(_) | Terminator::Branch { .. }
            ) && !block.instrs.iter().any(calls_compiled_code)
        })
        .collect();
    let successors: Vec<Vec<usize>> = f
        .blocks
        .iter()
        // Recovery edges are panic paths and emit no branch to orient.
        .map(super::lir_gen::lir_block_successors)
        .collect();
    let mut predecessors = vec![Vec::new(); n];
    for (block, edges) in successors.iter().enumerate() {
        for &target in edges {
            predecessors[target].push(block);
        }
    }
    // Iterative DFS from the entry; an edge into a block still on the stack
    // closes a loop at that header.
    let mut backedges: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut color = vec![0u8; n];
    if n > 0 {
        color[0] = 1;
        let mut stack = vec![(0usize, 0usize)];
        while let Some((block, next)) = stack.last_mut() {
            let block = *block;
            if *next == successors[block].len() {
                color[block] = 2;
                stack.pop();
                continue;
            }
            let target = successors[block][*next];
            *next += 1;
            match color[target] {
                0 => {
                    color[target] = 1;
                    stack.push((target, 0));
                }
                1 => backedges[target].push(block),
                _ => {}
            }
        }
    }
    let mut depths = LoopDepths {
        all: vec![0u32; n],
        call_free: vec![0u32; n],
    };
    // `mark[b] == header + 1` records membership in the current loop
    // without clearing a per-loop set.
    let mut mark = vec![0usize; n];
    let mut pending = Vec::new();
    let mut body = Vec::new();
    for (header, sources) in backedges.iter().enumerate() {
        if sources.is_empty() {
            continue;
        }
        mark[header] = header + 1;
        body.clear();
        body.push(header);
        pending.extend(sources.iter().copied());
        while let Some(block) = pending.pop() {
            if mark[block] == header + 1 {
                continue;
            }
            mark[block] = header + 1;
            body.push(block);
            pending.extend(predecessors[block].iter().copied());
        }
        let call_free = body.iter().all(|&block| block_call_free[block]);
        for &block in &body {
            depths.all[block] += 1;
            depths.call_free[block] += u32::from(call_free);
        }
    }
    depths
}

/// Whether a LIR `Branch` should be emitted with its targets swapped so the
/// `then` successor, which stays inside more call-free loops, is placed after
/// it.
pub(super) fn prefer_then_fallthrough(f: &LirFunction, depths: &LoopDepths, block: usize) -> bool {
    let Terminator::Branch {
        then_block,
        else_block,
        ..
    } = &f.blocks[block].terminator
    else {
        return false;
    };
    depths.call_free[then_block.0] > depths.call_free[else_block.0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CompilerOptions;
    use cranelift_codegen::dominator_tree::DominatorTree;
    use cranelift_codegen::flowgraph::ControlFlowGraph;
    use cranelift_codegen::ir::{Block, Opcode};

    fn compiled_functions(source: &str) -> Vec<Function> {
        CAPTURE.with(|capture| *capture.borrow_mut() = Some(Vec::new()));
        super::super::emit_interface::tests::compile_fixture_with(
            source,
            &CompilerOptions::release(),
        );
        CAPTURE.with(|capture| capture.borrow_mut().take().expect("capture active"))
    }

    /// Cranelift's emission order: the CFG reverse postorder with every
    /// effectively cold block sunk to the end (`BlockLoweringOrder` and
    /// `VCode` emission). Edge-split blocks are omitted.
    fn emission_order(func: &Function, domtree: &DominatorTree) -> Vec<Block> {
        let (hot, cold): (Vec<Block>, Vec<Block>) = domtree
            .cfg_rpo()
            .copied()
            .partition(|&block| !func.is_effectively_cold(block));
        hot.into_iter().chain(cold).collect()
    }

    struct LoopShape {
        hot_blocks: usize,
        contiguous: bool,
        hot_calls: usize,
        /// Memory reads and writes on the hot blocks, stack slots included.
        hot_loads: usize,
        hot_stores: usize,
        /// Hot branches on a value a call returned, such as a null test of
        /// the poll counter.
        call_result_branches: usize,
        unsigned_bounds: usize,
        signed_remainders: usize,
    }

    /// Every natural loop's hot blocks, their placement and their calls.
    fn loop_shapes(func: &Function) -> Vec<LoopShape> {
        let cfg = ControlFlowGraph::with_function(func);
        let domtree = DominatorTree::with_function(func, &cfg);
        let order = emission_order(func, &domtree);
        let position = |block: Block| order.iter().position(|&b| b == block).unwrap();
        let mut shapes = Vec::new();
        for &header in domtree.cfg_rpo() {
            let latches: Vec<Block> = cfg
                .pred_iter(header)
                .map(|pred| pred.block)
                .filter(|&pred| domtree.block_dominates(header, pred))
                .collect();
            if latches.is_empty() {
                continue;
            }
            let mut body = vec![header];
            let mut pending = latches;
            while let Some(block) = pending.pop() {
                if !body.contains(&block) {
                    body.push(block);
                    pending.extend(cfg.pred_iter(block).map(|pred| pred.block));
                }
            }
            let hot: Vec<Block> = body
                .into_iter()
                .filter(|&block| !func.is_effectively_cold(block))
                .collect();
            let mut positions: Vec<usize> = hot.iter().map(|&block| position(block)).collect();
            positions.sort_unstable();
            let insts: Vec<_> = hot
                .iter()
                .flat_map(|&block| func.layout.block_insts(block))
                .collect();
            let opcode = |inst| func.dfg.insts[inst].opcode();
            let call_results: Vec<_> = func
                .layout
                .blocks()
                .flat_map(|block| func.layout.block_insts(block))
                .filter(|&inst| opcode(inst).is_call())
                .flat_map(|inst| func.dfg.inst_results(inst).iter().copied())
                .collect();
            shapes.push(LoopShape {
                unsigned_bounds: insts
                    .iter()
                    .filter(|&&inst| {
                        func.dfg
                            .display_inst(inst)
                            .to_string()
                            .contains("icmp.i64 uge")
                    })
                    .count(),
                signed_remainders: insts
                    .iter()
                    .filter(|&&inst| opcode(inst) == Opcode::Srem)
                    .count(),
                hot_blocks: hot.len(),
                contiguous: positions.windows(2).all(|pair| pair[1] == pair[0] + 1),
                hot_calls: insts.iter().filter(|&&inst| opcode(inst).is_call()).count(),
                hot_loads: insts
                    .iter()
                    .filter(|&&inst| opcode(inst).can_load())
                    .count(),
                hot_stores: insts
                    .iter()
                    .filter(|&&inst| opcode(inst).can_store())
                    .count(),
                call_result_branches: insts
                    .iter()
                    .filter(|&&inst| opcode(inst) == Opcode::Brif)
                    .filter(|&&inst| {
                        let cond = func.dfg.inst_args(inst)[0];
                        call_results.contains(&cond)
                    })
                    .count(),
            });
        }
        shapes
    }

    fn assert_compact_loops(source: &str, expected_loops: usize) -> Vec<LoopShape> {
        let shapes: Vec<LoopShape> = compiled_functions(source)
            .iter()
            .flat_map(loop_shapes)
            .collect();
        assert_eq!(shapes.len(), expected_loops, "{source}");
        for shape in &shapes {
            assert!(shape.hot_blocks > 0);
            assert!(
                shape.contiguous,
                "a hot loop is split by other code:\n{source}"
            );
            assert_eq!(
                shape.hot_calls, 0,
                "a hot loop calls the runtime:\n{source}"
            );
            assert_eq!(
                shape.call_result_branches, 0,
                "a hot loop branches on a runtime call's result:\n{source}"
            );
        }
        shapes
    }

    // The array_read_sum_only phase (willow-nzsg): inline len/index slow paths
    // and the safepoint runtime calls must all sit outside the hot loop. Per
    // trip the read loop loads the GC stop flag, the countdown, the owner
    // from its root slot, the length, the buffer and the element, and stores
    // only the countdown: the receiver temporaries need no root stores, and
    // the index reuses the length and owner the condition loaded.
    #[test]
    fn array_read_loop_is_contiguous_and_call_free() {
        let shapes = assert_compact_loops(
            "import std::collections::Array;
            fn main() {
                let values: Array<i64> = [];
                let mut i: i64 = 0;
                while i < 1000 { values.push(i % 10); i = i + 1; }
                let mut sum: i64 = 0;
                i = 0;
                println(\"START\");
                while i < values.len() { sum = sum + values[i]; i = i + 1; }
                println(sum);
            }",
            2,
        );
        let read = shapes.last().expect("read loop");
        assert_eq!((read.hot_loads, read.hot_stores), (6, 1));
    }

    // The array_write_only phase (willow-ijui.10): the store's receiver copy
    // needs no root, and the store reuses the owner, length and buffer the
    // condition validated. Per trip: the GC stop flag, the countdown, the
    // owner, the length and the buffer are loaded; the countdown and the
    // element are stored.
    #[test]
    fn array_write_loop_reuses_condition_facts() {
        let shapes = assert_compact_loops(
            "import std::collections::Array;
            fn main() {
                let values: Array<i64> = [];
                let mut i: i64 = 0;
                while i < 1000 { values.push(0); i = i + 1; }
                i = 0;
                while i < values.len() { values[i] = (i % 1000) + 1; i = i + 1; }
                println(values[3]);
            }",
            2,
        );
        let write = shapes.last().expect("write loop");
        assert_eq!((write.hot_loads, write.hot_stores), (5, 2));
    }

    #[test]
    fn loop_exits_by_break_and_nesting_stay_outside_hot_bodies() {
        assert_compact_loops(
            "import std::collections::Array;
            fn main() {
                let values: Array<i64> = [1, 2, 3, 4];
                let mut total: i64 = 0;
                let mut i: i64 = 0;
                while true {
                    if i >= values.len() { break; }
                    let mut j: i64 = 0;
                    while j < values.len() { total = total + values[j] * values[i]; j = j + 1; }
                    i = i + 1;
                }
                println(total);
            }",
            2,
        );
    }
    #[test]
    fn nonnegative_loop_shares_proof_for_bounds_and_remainder() {
        for (initial, checks) in [("0", 0), ("-1", 1)] {
            let shapes = assert_compact_loops(
                &format!(
                    "import std::collections::Array;
                fn main() {{ let values: Array<i64> = [1,2,3]; let mut i = {initial};
                    while i < values.len() {{ values[i] = i % 1000; i = i + 1; }} }}"
                ),
                1,
            );
            assert_eq!(shapes[0].unsigned_bounds, checks, "initial={initial}");
            assert_eq!(shapes[0].signed_remainders, checks, "initial={initial}");
        }
    }
}
