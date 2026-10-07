//! Recovery-aware block liveness shared by last-use root clearing and async
//! frame planning.
//!
//! Sets are dense bit rows indexed by [`LirLocalId`]. The solver walks each
//! local backwards from its reads and records every live-in membership once,
//! so no pass rescans the control-flow graph to discover that nothing
//! changed and the cost does not depend on block order or loop nesting.

use std::collections::{HashMap, HashSet};

use super::async_liveness::{instruction_use_def, terminator_uses};
use super::{LirLocalId, SourceBlock, SourceTerminator};

#[cfg(test)]
thread_local! {
    /// Liveness solves on this thread, for duplicate-analysis tests.
    pub(crate) static SOLVES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One bit row of `words` 64-bit words per block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockSets {
    rows: usize,
    words: usize,
    bits: Vec<u64>,
}

impl BlockSets {
    fn new(blocks: usize, words: usize) -> Self {
        Self {
            rows: blocks,
            words,
            bits: vec![0; blocks * words],
        }
    }

    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    pub(crate) fn row(&self, block: usize) -> &[u64] {
        &self.bits[block * self.words..(block + 1) * self.words]
    }

    fn row_mut(&mut self, block: usize) -> &mut [u64] {
        &mut self.bits[block * self.words..(block + 1) * self.words]
    }

    fn push_row(&mut self, row: &[u64]) {
        self.rows += 1;
        self.bits.extend_from_slice(row);
    }

    pub(crate) fn iter(&self, block: usize) -> impl Iterator<Item = LirLocalId> + '_ {
        ones(self.row(block))
    }
}

pub(crate) fn contains(row: &[u64], id: LirLocalId) -> bool {
    let index = id.0 as usize;
    row[index / 64] & (1 << (index % 64)) != 0
}

pub(crate) fn insert(row: &mut [u64], id: LirLocalId) {
    let index = id.0 as usize;
    row[index / 64] |= 1 << (index % 64);
}

pub(crate) fn remove(row: &mut [u64], id: LirLocalId) {
    let index = id.0 as usize;
    row[index / 64] &= !(1 << (index % 64));
}

/// Set members in ascending id order.
pub(crate) fn ones(row: &[u64]) -> impl Iterator<Item = LirLocalId> + '_ {
    row.iter().enumerate().flat_map(|(word, &bits)| {
        let mut bits = bits;
        std::iter::from_fn(move || {
            (bits != 0).then(|| {
                let bit = bits.trailing_zeros();
                bits &= bits - 1;
                LirLocalId((word * 64) as u32 + bit)
            })
        })
    })
}

/// Deterministic work counters for scaling tests and audit reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LivenessStats {
    /// `(block, local)` live-in memberships discovered; each is found once.
    pub entries: usize,
    /// Predecessor edges examined, once per membership of the edge target.
    pub edge_visits: usize,
}

/// Block-level use/def and live-in/live-out sets for one LIR body.
///
/// `live_in[b]` also contains everything live into each recovery target of
/// `b`: a panic may leave before any definition in `b`, so a later write in
/// the block cannot kill a value the recovery continuation reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Liveness {
    pub uses: BlockSets,
    pub defs: BlockSets,
    pub live_in: BlockSets,
    pub live_out: BlockSets,
    pub stats: LivenessStats,
}

impl Liveness {
    #[cfg(test)]
    pub(crate) fn compute(blocks: &[SourceBlock], locals: &[super::LirLocal]) -> Self {
        let names: HashMap<&str, LirLocalId> = locals
            .iter()
            .map(|local| (local.name.as_str(), local.id))
            .collect();
        Self::compute_named(blocks, locals.len(), &names)
    }

    pub(crate) fn compute_named(
        blocks: &[SourceBlock],
        local_count: usize,
        names: &HashMap<&str, LirLocalId>,
    ) -> Self {
        let words = local_count.div_ceil(64);
        let mut uses = BlockSets::new(blocks.len(), words);
        let mut defs = BlockSets::new(blocks.len(), words);
        let mut preds = vec![Vec::new(); blocks.len()];
        let mut read = HashSet::new();
        let mut written = HashSet::new();
        for (index, block) in blocks.iter().enumerate() {
            debug_assert_eq!(block.id.0, index, "LIR block ids are dense indexes");
            read.clear();
            written.clear();
            for inst in &block.instrs {
                instruction_use_def(inst, names, &mut read, &mut written);
            }
            terminator_uses(&block.terminator, names, &mut read, &written);
            let row = uses.row_mut(index);
            read.iter().for_each(|&id| insert(row, id));
            let row = defs.row_mut(index);
            written.iter().for_each(|&id| insert(row, id));
            for target in normal_successors(&block.terminator) {
                preds[target].push((index, false));
            }
            for target in &block.recovery {
                preds[target.0].push((index, true));
            }
        }
        solve(uses, defs, &preds)
    }

    /// Record a block appended after solving whose only successor is
    /// `target` and which neither reads nor writes a local (an edge block
    /// holding root clears). Existing rows are unchanged: the predecessor
    /// edge now reaches an identical live-in set.
    pub(crate) fn push_forwarding_block(&mut self, target: usize) {
        let empty = vec![0; self.uses.words];
        self.uses.push_row(&empty);
        self.defs.push_row(&empty);
        let live = self.live_in.row(target).to_vec();
        self.live_in.push_row(&live);
        self.live_out.push_row(&live);
    }

    #[cfg(test)]
    /// Approximate resident bytes of the four retained set tables.
    pub(crate) fn set_bytes(&self) -> usize {
        8 * (self.uses.bits.len()
            + self.defs.bits.len()
            + self.live_in.bits.len()
            + self.live_out.bits.len())
    }
}

pub(crate) fn normal_successors(terminator: &SourceTerminator) -> impl Iterator<Item = usize> {
    let (first, second) = match terminator {
        SourceTerminator::Jump(target) | SourceTerminator::Suspend { resume: target, .. } => {
            (Some(target.0), None)
        }
        SourceTerminator::Branch {
            then_block,
            else_block,
            ..
        } => (Some(then_block.0), Some(else_block.0)),
        SourceTerminator::Return(_) | SourceTerminator::CleanupReturn => (None, None),
    };
    first.into_iter().chain(second)
}

/// Least fixed point of
/// `in[b] = use[b] | (out[b] & !def[b]) | in[r] for recovery r`,
/// `out[b] = in[s]` over all successors `s`, including recovery targets.
///
/// Sparse per-local exploration: walk backwards from each upward-exposed
/// read, adding the local to a block's live-in at most once and examining
/// each predecessor edge of that block once. Work is
/// `O(B * W + sum(|in[b]| * indegree(b)))`, the size of the result times the
/// edges that carry it, independent of block order, loop nesting depth and
/// how many distinct reads feed one region. The stack holds at most one
/// entry per block.
fn solve(uses: BlockSets, defs: BlockSets, preds: &[Vec<(usize, bool)>]) -> Liveness {
    #[cfg(test)]
    SOLVES.with(|count| count.set(count.get() + 1));
    let blocks = preds.len();
    let words = uses.words;
    let mut stats = LivenessStats::default();
    // Transpose upward-exposed reads to per-local block lists.
    let locals = words * 64;
    let mut starts = vec![0usize; locals + 1];
    for block in 0..blocks {
        for id in uses.iter(block) {
            starts[id.0 as usize + 1] += 1;
        }
    }
    for local in 0..locals {
        starts[local + 1] += starts[local];
    }
    let mut readers = vec![0usize; starts[locals]];
    let mut fill = starts.clone();
    for block in 0..blocks {
        for id in uses.iter(block) {
            readers[fill[id.0 as usize]] = block;
            fill[id.0 as usize] += 1;
        }
    }
    let mut live_in = uses.clone();
    let mut live_out = BlockSets::new(blocks, words);
    let mut stack = Vec::new();
    for local in 0..locals {
        let id = LirLocalId(local as u32);
        stack.extend_from_slice(&readers[starts[local]..starts[local + 1]]);
        while let Some(block) = stack.pop() {
            stats.entries += 1;
            for &(pred, recovery) in &preds[block] {
                stats.edge_visits += 1;
                insert(live_out.row_mut(pred), id);
                if (recovery || !contains(defs.row(pred), id)) && !contains(live_in.row(pred), id) {
                    insert(live_in.row_mut(pred), id);
                    stack.push(pred);
                }
            }
        }
    }
    Liveness {
        uses,
        defs,
        live_in,
        live_out,
        stats,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Graph {
        uses: Vec<Vec<u32>>,
        defs: Vec<Vec<u32>>,
        normal: Vec<Vec<usize>>,
        recovery: Vec<Vec<usize>>,
        locals: usize,
    }

    impl Graph {
        fn new(blocks: usize, locals: usize) -> Self {
            Self {
                uses: vec![Vec::new(); blocks],
                defs: vec![Vec::new(); blocks],
                normal: vec![Vec::new(); blocks],
                recovery: vec![Vec::new(); blocks],
                locals,
            }
        }

        fn solve(&self) -> Liveness {
            let words = self.locals.div_ceil(64);
            let blocks = self.uses.len();
            let mut uses = BlockSets::new(blocks, words);
            let mut defs = BlockSets::new(blocks, words);
            let mut preds = vec![Vec::new(); blocks];
            for block in 0..blocks {
                for &id in &self.uses[block] {
                    insert(uses.row_mut(block), LirLocalId(id));
                }
                for &id in &self.defs[block] {
                    insert(defs.row_mut(block), LirLocalId(id));
                }
                for &target in &self.normal[block] {
                    preds[target].push((block, false));
                }
                for &target in &self.recovery[block] {
                    preds[target].push((block, true));
                }
            }
            solve(uses, defs, &preds)
        }

        /// The previous algorithm: rescan every block in reverse id order
        /// until a whole round changes nothing. Returns its sets and rounds.
        fn reference(&self) -> (Vec<HashSet<u32>>, Vec<HashSet<u32>>, usize) {
            let blocks = self.uses.len();
            let uses: Vec<HashSet<u32>> = self
                .uses
                .iter()
                .map(|u| u.iter().copied().collect())
                .collect();
            let defs: Vec<HashSet<u32>> = self
                .defs
                .iter()
                .map(|d| d.iter().copied().collect())
                .collect();
            let mut live_in = uses.clone();
            let mut live_out = vec![HashSet::new(); blocks];
            let mut rounds = 0;
            loop {
                rounds += 1;
                let mut changed = false;
                for block in (0..blocks).rev() {
                    let out: HashSet<u32> = self.normal[block]
                        .iter()
                        .chain(&self.recovery[block])
                        .flat_map(|&s| live_in[s].iter().copied())
                        .collect();
                    let mut input = uses[block].clone();
                    input.extend(out.difference(&defs[block]).copied());
                    for &r in &self.recovery[block] {
                        input.extend(live_in[r].iter().copied());
                    }
                    changed |= input != live_in[block] || out != live_out[block];
                    live_in[block] = input;
                    live_out[block] = out;
                }
                if !changed {
                    return (live_in, live_out, rounds);
                }
            }
        }

        fn check(&self) -> (Liveness, usize) {
            let live = self.solve();
            let (live_in, live_out, rounds) = self.reference();
            for block in 0..self.uses.len() {
                let row =
                    |sets: &BlockSets| sets.iter(block).map(|id| id.0).collect::<HashSet<_>>();
                assert_eq!(row(&live.live_in), live_in[block], "live-in of bb{block}");
                assert_eq!(
                    row(&live.live_out),
                    live_out[block],
                    "live-out of bb{block}"
                );
            }
            (live, rounds)
        }
    }

    /// Entry is the highest id and each block jumps to the next lower id;
    /// the only read is in bb0. Reverse-order rescans move one block per
    /// round; the worklist moves the bit once along each edge.
    fn reversed_chain(blocks: usize) -> Graph {
        let mut graph = Graph::new(blocks, 1);
        for block in 1..blocks {
            graph.normal[block].push(block - 1);
        }
        graph.uses[0].push(0);
        graph.defs[blocks - 1].push(0);
        graph
    }

    /// A loop header at bb0 whose body chain bb1..bbN-1 jumps back to bb0
    /// and exits to bbN. The latch reads a local the header does not, so it
    /// becomes live around the whole loop only through the backedge.
    fn loop_body(blocks: usize) -> Graph {
        let mut graph = Graph::new(blocks + 1, 2);
        graph.normal[0] = vec![1, blocks];
        for block in 1..blocks {
            graph.normal[block].push(if block + 1 == blocks { 0 } else { block + 1 });
        }
        graph.uses[0].push(1);
        graph.uses[blocks - 1].push(0);
        graph.uses[blocks].push(1);
        graph
    }

    /// `locals` values defined in bb0 and all read in the last block of a
    /// forward chain of `blocks` blocks.
    fn wide(blocks: usize, locals: usize) -> Graph {
        let mut graph = Graph::new(blocks, locals);
        for block in 0..blocks - 1 {
            graph.normal[block].push(block + 1);
        }
        graph.defs[0] = (0..locals as u32).collect();
        graph.uses[blocks - 1] = (0..locals as u32).collect();
        graph
    }

    /// `fanout` straight-line blocks share one recovery handler (the last
    /// block) that reads a local each of them redefines.
    fn recovery_fanout(fanout: usize) -> Graph {
        let handler = fanout;
        let mut graph = Graph::new(fanout + 1, 2);
        for block in 0..fanout {
            if block + 1 < fanout {
                graph.normal[block].push(block + 1);
            }
            graph.recovery[block].push(handler);
            graph.defs[block].push(0);
            graph.uses[block].push(1);
        }
        graph.uses[handler].push(0);
        graph
    }

    /// Check the solver against the reference at each size, assert that its
    /// work equals the result size times the carrying edges, and that this
    /// work grows linearly with `size`.
    fn assert_linear(name: &str, sizes: &[usize], build: impl Fn(usize) -> Graph) {
        let mut previous: Option<f64> = None;
        for &size in sizes {
            let graph = build(size);
            let blocks = graph.uses.len();
            let edges: usize = graph
                .normal
                .iter()
                .chain(&graph.recovery)
                .map(Vec::len)
                .sum();
            let (live, rounds) = graph.check();
            let mut indegree = vec![0; blocks];
            for target in graph.normal.iter().chain(&graph.recovery).flatten() {
                indegree[*target] += 1;
            }
            let memberships: usize = (0..blocks).map(|b| live.live_in.iter(b).count()).sum();
            let carried: usize = (0..blocks)
                .map(|b| live.live_in.iter(b).count() * indegree[b])
                .sum();
            println!(
                "{name} size={size} blocks={blocks} edges={edges} locals={} live_in_entries={} edge_visits={} reference_rounds={rounds} reference_block_scans={} reference_edge_scans={}",
                graph.locals,
                live.stats.entries,
                live.stats.edge_visits,
                rounds * blocks,
                rounds * edges,
            );
            assert_eq!(live.stats.entries, memberships, "{name} {size}");
            assert_eq!(live.stats.edge_visits, carried, "{name} {size}");
            // Visits per unit of size stay flat (10% slack for fixed blocks).
            let per_size = live.stats.edge_visits as f64 / size as f64;
            if let Some(previous) = previous {
                assert!(
                    per_size <= previous * 1.1,
                    "{name} {size}: {per_size} after {previous}"
                );
            }
            previous = Some(per_size);
        }
    }

    #[test]
    fn reversed_chain_visits_each_edge_once() {
        assert_linear("reversed-chain", &[16, 64, 256, 1024], reversed_chain);
        let (live, rounds) = reversed_chain(1024).check();
        assert_eq!(live.stats.edge_visits, 1023);
        // The previous fixed point moved the read one block per round and
        // rescanned all 1024 blocks each time (1023 rounds plus a final
        // no-change round).
        assert_eq!(rounds, 1024);
    }

    #[test]
    fn loop_backedge_propagates_without_rescans() {
        assert_linear("loop", &[16, 64, 256, 1024], loop_body);
        let (live, _) = loop_body(1024).check();
        // Both locals are live into every loop block exactly once.
        assert_eq!(live.stats.entries, 2 * 1024 + 1);
    }

    #[test]
    fn wide_live_sets_record_each_membership_once() {
        assert_linear("wide-locals", &[64, 256, 1024, 4096], |locals| {
            wide(32, locals)
        });
        assert_linear("wide-blocks", &[16, 64, 256, 1024], |blocks| {
            wide(blocks, 256)
        });
        let (live, _) = wide(32, 4096).check();
        assert_eq!(live.stats.edge_visits, 31 * 4096);
    }

    #[test]
    fn recovery_fanout_keeps_handler_reads_live_across_redefinitions() {
        assert_linear("recovery-fanout", &[16, 64, 256, 1024], recovery_fanout);
        let live = recovery_fanout(4).solve();
        for block in 0..4 {
            assert!(
                contains(live.live_in.row(block), LirLocalId(0)),
                "bb{block}"
            );
        }
    }

    #[test]
    fn matches_reference_on_irreducible_and_self_loop_shapes() {
        // Deterministic pseudo-random graphs with self loops, duplicate
        // branch targets, shared recovery handlers and irreducible cycles.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |limit: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % limit as u64) as usize
        };
        for _ in 0..200 {
            let blocks = 1 + next(24);
            let locals = 1 + next(130);
            let mut graph = Graph::new(blocks, locals);
            for block in 0..blocks {
                for _ in 0..next(3) {
                    graph.normal[block].push(next(blocks));
                }
                if next(4) == 0 {
                    graph.recovery[block].push(next(blocks));
                }
                for _ in 0..next(4) {
                    graph.uses[block].push(next(locals) as u32);
                }
                for _ in 0..next(4) {
                    graph.defs[block].push(next(locals) as u32);
                }
            }
            graph.check();
        }
    }

    #[test]
    fn forwarding_block_reuses_target_live_in() {
        let mut live = wide(3, 70).solve();
        live.push_forwarding_block(2);
        assert_eq!(live.live_in.row(3), live.live_in.row(2));
        assert_eq!(live.live_out.row(3), live.live_in.row(2));
        assert!(live.uses.row(3).iter().all(|&word| word == 0));
        assert_eq!(live.set_bytes(), 4 * 4 * 2 * 8);
    }

    #[test]
    fn bit_rows_iterate_in_ascending_order() {
        let mut row = vec![0u64; 3];
        for id in [129, 0, 64, 63, 1] {
            insert(&mut row, LirLocalId(id));
        }
        remove(&mut row, LirLocalId(1));
        let ids: Vec<u32> = ones(&row).map(|id| id.0).collect();
        assert_eq!(ids, [0, 63, 64, 129]);
        assert!(contains(&row, LirLocalId(129)) && !contains(&row, LirLocalId(1)));
    }
}
