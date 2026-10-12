//! E2604 through function values (willow-h4t7).
//!
//! A call through a `fn`/`closure` value has no static target, so the lock
//! effect graph cannot see what it runs. Instead the checker keeps an
//! address-taken set: every function value that a program creates (a named
//! function used as a value, or a lambda) is recorded with its arity, and every
//! indirect call is recorded with the arity it calls with. An indirect call can
//! only reach a function value of the same arity, so a lock-held indirect call,
//! direct or through a helper, is rejected exactly when some function value of
//! that arity may suspend or block before returning.
//!
//! Arity is the only key: it is exact at both ends (a call's argument count and
//! a value's parameter count), stable across modules, aliases and generic
//! instantiation, and coarsening a key only adds candidates, which is sound.
//! Keys are bits of a `u64` (arities of 63 and more share the last bit), so
//! every set operation below is O(1).

use std::collections::HashMap;

use crate::diagnostics::Span;
use crate::semantic::ids::FunctionId;

/// The key bit of a call or value with `arity` parameters.
pub(crate) fn arity_bit(arity: usize) -> u64 {
    1 << arity.min(63)
}

/// One function value created in a body.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct FunctionValue {
    /// `None` for a lambda without a callable identity, which is assumed to
    /// wait.
    pub(crate) target: Option<FunctionId>,
    pub(crate) bit: u64,
    pub(crate) span: Span,
}

/// What one body contributes, kept in source order so that replaying a cached
/// body reproduces the inline result.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct IndirectInputs {
    pub(crate) values: Vec<FunctionValue>,
    /// Arity bits of the indirect calls each callable makes itself.
    pub(crate) calls: Vec<(FunctionId, u64)>,
    /// Indirect calls made while holding a lock.
    pub(crate) lock_sites: Vec<(Span, u64)>,
}

impl IndirectInputs {
    pub(crate) fn extend(&mut self, other: IndirectInputs) {
        self.values.extend(other.values);
        self.calls.extend(other.calls);
        self.lock_sites.extend(other.lock_sites);
    }

    pub(crate) fn clear(&mut self) {
        self.values.clear();
        self.calls.clear();
        self.lock_sites.clear();
    }
}

/// Which arities hold a waiting function value. A value that only waits
/// through its own indirect calls is conditional: it waits once one of the
/// arities it reaches does. Conditions are folded per arity bit, because some
/// value of arity `b` reaching a waiting arity is the same as the union of
/// their reaches intersecting the waiting set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValueMasks {
    pub(crate) waiting: u64,
    pub(crate) conditional: [u64; 64],
}

impl Default for ValueMasks {
    fn default() -> Self {
        Self {
            waiting: 0,
            conditional: [0; 64],
        }
    }
}

impl ValueMasks {
    pub(crate) fn add_conditional(&mut self, bit: u64, reach: u64) {
        if reach != 0 {
            self.conditional[bit.trailing_zeros() as usize] |= reach;
        }
    }

    pub(crate) fn merge(&mut self, other: &ValueMasks) {
        self.waiting |= other.waiting;
        for (mine, theirs) in self.conditional.iter_mut().zip(other.conditional) {
            *mine |= theirs;
        }
    }

    /// The closed waiting set. Every round adds a bit or stops, so this is at
    /// most 64 rounds of 64 tests regardless of program size.
    pub(crate) fn closure(&self) -> u64 {
        let mut waiting = self.waiting;
        loop {
            let mut next = waiting;
            for (bit, reach) in self.conditional.iter().enumerate() {
                if reach & waiting != 0 {
                    next |= 1 << bit;
                }
            }
            if next == waiting {
                return waiting;
            }
            waiting = next;
        }
    }
}

/// Close `reach` over the synchronous call graph: a callable reaches the
/// indirect calls of everything it calls. `reach` starts with each node's own
/// bits plus those of its targets in earlier units; `edges` are the local
/// `(caller, callee)` pairs. Every node's mask grows at most 64 times, so the
/// worklist does O(64 * edges) relaxations.
pub(crate) fn close_reach(
    reach: HashMap<FunctionId, u64>,
    edges: impl Iterator<Item = (FunctionId, FunctionId)>,
) -> HashMap<FunctionId, u64> {
    close_reach_counted(reach, edges).0
}

fn close_reach_counted(
    mut reach: HashMap<FunctionId, u64>,
    edges: impl Iterator<Item = (FunctionId, FunctionId)>,
) -> (HashMap<FunctionId, u64>, usize) {
    let mut callers: HashMap<FunctionId, Vec<FunctionId>> = HashMap::new();
    for (caller, callee) in edges {
        callers.entry(callee).or_default().push(caller);
    }
    let mut work: Vec<FunctionId> = reach
        .iter()
        .filter(|(_, mask)| **mask != 0)
        .map(|(id, _)| *id)
        .collect();
    let mut relaxations = 0;
    while let Some(callee) = work.pop() {
        let mask = reach.get(&callee).copied().unwrap_or(0);
        for caller in callers.get(&callee).into_iter().flatten() {
            relaxations += 1;
            let current = reach.entry(*caller).or_default();
            if *current | mask != *current {
                *current |= mask;
                work.push(*caller);
            }
        }
    }
    reach.retain(|_, mask| *mask != 0);
    (reach, relaxations)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> FunctionId {
        FunctionId::free_from_source_name(name)
    }

    #[test]
    fn arity_bits_saturate() {
        assert_eq!(arity_bit(0), 1);
        assert_eq!(arity_bit(2), 4);
        assert_eq!(arity_bit(63), arity_bit(200));
    }

    #[test]
    fn conditional_values_close_transitively() {
        let mut masks = ValueMasks {
            waiting: arity_bit(1),
            ..Default::default()
        };
        // An arity-2 value calls an arity-1 value, an arity-3 value calls an
        // arity-2 value, and an arity-4 value calls only an arity-5 value.
        masks.add_conditional(arity_bit(2), arity_bit(1));
        masks.add_conditional(arity_bit(3), arity_bit(2));
        masks.add_conditional(arity_bit(4), arity_bit(5));
        assert_eq!(masks.closure(), arity_bit(1) | arity_bit(2) | arity_bit(3));
    }

    #[test]
    fn reach_closes_over_chains_and_cycles() {
        let seed = HashMap::from([(id("leaf"), arity_bit(1)), (id("loop_b"), arity_bit(2))]);
        let edges = [
            (id("top"), id("mid")),
            (id("mid"), id("leaf")),
            (id("loop_a"), id("loop_b")),
            (id("loop_b"), id("loop_a")),
        ];
        let reach = close_reach(seed, edges.into_iter());
        assert_eq!(reach[&id("top")], arity_bit(1));
        assert_eq!(reach[&id("loop_a")], arity_bit(2));
        assert_eq!(reach[&id("loop_b")], arity_bit(2));
        assert!(!reach.contains_key(&id("unrelated")));
    }

    /// Deterministic work bound: chains and fan-in graphs where every node
    /// adds one of 64 bits converge within 64 relaxations per edge, so the
    /// count grows linearly with the edge count.
    #[test]
    fn reach_work_is_linear_in_edges() {
        for n in [100usize, 1_000, 10_000] {
            let names: Vec<_> = (0..n).map(|i| id(&format!("f{i}"))).collect();
            let seed: HashMap<_, _> = names
                .iter()
                .enumerate()
                .map(|(i, name)| (*name, arity_bit(i % 64)))
                .collect();
            let chain = names.windows(2).map(|pair| (pair[0], pair[1]));
            let (reach, relaxations) = close_reach_counted(seed.clone(), chain);
            assert_eq!(reach[&names[0]], u64::MAX);
            assert!(relaxations <= 64 * (n - 1), "chain {n}: {relaxations}");
            println!(
                "indirect reach chain n={n} edges={} relaxations={relaxations}",
                n - 1
            );
            // Fan-in: every node calls the same hub, which calls them all.
            let hub = id("hub");
            let fan = names.iter().flat_map(|name| [(*name, hub), (hub, *name)]);
            let (reach, relaxations) = close_reach_counted(seed, fan);
            assert_eq!(reach[&hub], u64::MAX);
            assert!(relaxations <= 64 * 2 * n, "fan {n}: {relaxations}");
            println!(
                "indirect reach fan n={n} edges={} relaxations={relaxations}",
                2 * n
            );
        }
    }
}
