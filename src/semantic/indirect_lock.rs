//! Shared bounded function-value facts. Local calls use inclusion constraints;
//! unresolved and cross-unit calls retain full-signature address-taken bounds.
//! Arity masks remain a constant-size conservative transitive waiting summary.

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
    /// Nominal identity of the value's parameter and return types (see
    /// [`IndirectCall::key`]); `None` when some part has no stable identity.
    #[serde(default)]
    pub(crate) key: Option<String>,
}

/// An indirect call made by `caller` itself.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct IndirectCall {
    pub(crate) caller: FunctionId,
    pub(crate) bit: u64,
    /// Full invariant function signature keyed by declaration identities.
    /// An unresolved signature matches every value of the arity.
    #[serde(default)]
    pub(crate) key: Option<String>,
    #[serde(default = "Span::dummy")]
    pub(crate) span: Span,
    /// Computed once from the unit's value-flow constraints. None retains the
    /// typed address-taken bound (including future importing units).
    #[serde(default)]
    pub(crate) targets: Option<Vec<FunctionId>>,
    #[serde(default)]
    pub(crate) unknown_reasons: Vec<String>,
}

impl IndirectCall {
    pub(crate) fn bound(&self) -> FunctionId {
        FunctionId::synthetic_free(
            format!("$value-bound:{}:{:?}", self.bit.trailing_zeros(), self.key),
            crate::qualified_name::SyntheticKind::Indirect {
                slot: self.bit.trailing_zeros() as u8,
                wildcard: self.key.is_none(),
            },
        )
    }
}

/// What one body contributes, kept in source order so that replaying a cached
/// body reproduces the inline result.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct IndirectInputs {
    pub(crate) values: Vec<FunctionValue>,
    /// The indirect calls each callable makes itself.
    pub(crate) calls: Vec<IndirectCall>,
    /// Checked memberwise field order, including inherited fields, per call.
    #[serde(default)]
    pub(crate) memberwise: Vec<(Span, Vec<String>)>,
    /// Resolved base-constructor calls have statement spans, not expression IDs.
    #[serde(default)]
    pub(crate) super_calls: Vec<(Span, FunctionId)>,
    /// Indirect calls made while holding a lock.
    pub(crate) lock_sites: Vec<(Span, u64)>,
}

impl IndirectInputs {
    pub(crate) fn extend(&mut self, other: IndirectInputs) {
        self.values.extend(other.values);
        self.calls.extend(other.calls);
        self.super_calls.extend(other.super_calls);
        self.memberwise.extend(other.memberwise);
        self.lock_sites.extend(other.lock_sites);
    }

    pub(crate) fn clear(&mut self) {
        self.values.clear();
        self.calls.clear();
        self.super_calls.clear();
        self.memberwise.clear();
        self.lock_sites.clear();
    }
}

/// Cross-unit signature reach, bounded independently of the number of types.
/// Overflow widens just the affected arities. At most 32 exact keys and 64
/// wildcard bits can be added; union and compatibility take bounded work.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TypeReach {
    keys: std::collections::BTreeSet<(u64, String)>,
    wild: u64,
}
impl TypeReach {
    pub(crate) fn add(&mut self, bit: u64, key: Option<&str>) {
        if let Some(key) = key {
            if self.keys.len() < 32 {
                self.keys.insert((bit, key.into()));
            } else if !self.keys.contains(&(bit, key.into())) {
                self.wild |= bit;
            }
        } else {
            self.wild |= bit;
        }
    }
    pub(crate) fn merge(&mut self, other: &Self) -> bool {
        let before = (self.keys.len(), self.wild);
        self.wild |= other.wild;
        for (bit, key) in &other.keys {
            self.add(*bit, Some(key));
        }
        before != (self.keys.len(), self.wild)
    }
    pub(crate) fn intersects(&self, other: &Self, bits: u64) -> bool {
        self.wild & other.wild & bits != 0
            || self.keys.iter().any(|(bit, key)| {
                bit & bits != 0
                    && (other.wild & bit != 0 || other.keys.contains(&(*bit, key.clone())))
            })
            || other
                .keys
                .iter()
                .any(|(bit, _)| self.wild & bit & bits != 0)
    }
}

/// Source witnesses indexed by full signature. Each query visits at most the
/// bounded reach's 32 exact keys and 64 arity bits, never all stored values.
#[derive(Default)]
pub(crate) struct TypeWitnesses {
    exact: HashMap<u64, HashMap<String, Span>>,
    wildcard: HashMap<u64, Span>,
    any: HashMap<u64, Span>,
    #[cfg(test)]
    probes: std::cell::Cell<usize>,
}
impl TypeWitnesses {
    pub(crate) fn add(&mut self, bit: u64, key: Option<&str>, span: Span) {
        if let Some(key) = key {
            self.exact
                .entry(bit)
                .or_default()
                .entry(key.into())
                .or_insert(span);
        } else {
            self.wildcard.entry(bit).or_insert(span);
        }
        self.any.entry(bit).or_insert(span);
    }
    pub(crate) fn merge(&mut self, other: Self) {
        for (bit, entries) in other.exact {
            let destination = self.exact.entry(bit).or_default();
            for (key, span) in entries {
                destination.entry(key).or_insert(span);
            }
        }
        for (bit, span) in other.wildcard {
            self.wildcard.entry(bit).or_insert(span);
        }
        for (bit, span) in other.any {
            self.any.entry(bit).or_insert(span);
        }
    }
    pub(crate) fn matching(&self, reach: &TypeReach, bits: u64) -> Option<Span> {
        for (bit, key) in &reach.keys {
            #[cfg(test)]
            self.probes.set(self.probes.get() + 1);
            if bit & bits != 0
                && let Some(span) = self
                    .exact
                    .get(bit)
                    .and_then(|entries| entries.get(key))
                    .or_else(|| self.wildcard.get(bit))
            {
                return Some(*span);
            }
        }
        let mut remaining = reach.wild & bits;
        while remaining != 0 {
            #[cfg(test)]
            self.probes.set(self.probes.get() + 1);
            let bit = 1 << remaining.trailing_zeros();
            if let Some(span) = self.any.get(&bit) {
                return Some(*span);
            }
            remaining &= !bit;
        }
        None
    }
}

/// Bounded typed waiting closure for values that themselves invoke callbacks.
/// Keep 32 exact dependency keys plus one wildcard row per arity. Widening
/// never drops a dependency; the fixed-size closure cannot scale quadratically
/// with the program's number of function values.
#[derive(Default, Clone)]
pub(crate) struct TypeValues {
    pub(crate) waiting: TypeReach,
    exact: std::collections::BTreeMap<(u64, String), TypeReach>,
    wide: HashMap<u64, TypeReach>,
}
impl TypeValues {
    pub(crate) fn conditional(&mut self, bit: u64, key: Option<&str>, reach: &TypeReach) {
        if let Some(key) = key {
            let key = (bit, key.to_owned());
            if self.exact.len() < 32 || self.exact.contains_key(&key) {
                self.exact.entry(key).or_default().merge(reach);
                return;
            }
        }
        self.wide.entry(bit).or_default().merge(reach);
    }
    pub(crate) fn merge(&mut self, other: &Self) {
        self.waiting.merge(&other.waiting);
        for ((bit, key), reach) in &other.exact {
            self.conditional(*bit, Some(key), reach);
        }
        for (&bit, reach) in &other.wide {
            self.conditional(bit, None, reach);
        }
    }
    pub(crate) fn closure(&self) -> TypeReach {
        let mut waiting = self.waiting.clone();
        loop {
            let before = waiting.clone();
            for ((bit, key), reach) in &self.exact {
                if reach.intersects(&waiting, u64::MAX) {
                    waiting.add(*bit, Some(key));
                }
            }
            for (&bit, reach) in &self.wide {
                if reach.intersects(&waiting, u64::MAX) {
                    waiting.add(bit, None);
                }
            }
            if before == waiting {
                return waiting;
            }
        }
    }
}

pub(crate) fn close_types(
    reach: HashMap<FunctionId, TypeReach>,
    edges: impl Iterator<Item = (FunctionId, FunctionId)>,
) -> HashMap<FunctionId, TypeReach> {
    close_types_counted(reach, edges).0
}
fn close_types_counted(
    mut reach: HashMap<FunctionId, TypeReach>,
    edges: impl Iterator<Item = (FunctionId, FunctionId)>,
) -> (HashMap<FunctionId, TypeReach>, usize) {
    let mut operations = 0;
    let mut callers: HashMap<_, Vec<_>> = HashMap::new();
    for (caller, callee) in edges {
        callers.entry(callee).or_default().push(caller);
    }
    let mut work: Vec<_> = reach.keys().copied().collect();
    while let Some(callee) = work.pop() {
        let keys = reach[&callee].clone();
        for caller in callers.get(&callee).into_iter().flatten() {
            operations += 1;
            if reach.entry(*caller).or_default().merge(&keys) {
                work.push(*caller);
            }
        }
    }
    (reach, operations)
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
    #[test]
    fn function_flow_cross_unit_type_work_is_bounded() {
        for n in [64, 256, 1024] {
            for cyclic in [false, true] {
                let ids: Vec<_> = (0..n).map(|i| id(&format!("typed{i}"))).collect();
                let seed = ids
                    .iter()
                    .enumerate()
                    .map(|(i, &id)| {
                        let mut keys = TypeReach::default();
                        keys.add(arity_bit(i % 64), Some(&format!("type{i}")));
                        (id, keys)
                    })
                    .collect();
                let mut edges: Vec<_> = ids.windows(2).map(|p| (p[0], p[1])).collect();
                if cyclic {
                    edges.push((ids[n - 1], ids[0]));
                }
                let (reach, operations) = close_types_counted(seed, edges.iter().copied());
                assert!(reach.values().all(|r| r.keys.len() <= 32));
                assert!(operations <= 97 * edges.len(), "{n}: {operations}");
                println!(
                    "flow type-reach nodes={n} cycle={cyclic} edges={} merges={operations} key-visits-bound={}",
                    edges.len(),
                    32 * operations
                );
                for i in 0..n {
                    let mut expected = TypeReach::default();
                    expected.add(arity_bit(i % 64), Some(&format!("type{i}")));
                    assert!(reach[&ids[0]].intersects(&expected, u64::MAX));
                }
            }
        }
    }
    #[test]
    fn function_flow_typed_conditional_waits_preserve_signatures() {
        let mut values = TypeValues::default();
        values.waiting.add(arity_bit(1), Some("bool->i64"));
        let mut reach = TypeReach::default();
        reach.add(arity_bit(1), Some("i64->i64"));
        values.conditional(arity_bit(0), Some("()->i64"), &reach);
        let mut callback = TypeReach::default();
        callback.add(arity_bit(0), Some("()->i64"));
        assert!(!values.closure().intersects(&callback, u64::MAX));
        values.waiting.add(arity_bit(1), Some("i64->i64"));
        assert!(values.closure().intersects(&callback, u64::MAX));
        for i in 0..128 {
            values.conditional(arity_bit(i % 64), Some(&format!("type{i}")), &reach);
        }
        assert!(values.exact.len() <= 32 && values.wide.len() <= 64);
        for i in 0..128 {
            let mut key = TypeReach::default();
            key.add(arity_bit(i % 64), Some(&format!("type{i}")));
            assert!(values.closure().intersects(&key, u64::MAX));
        }
    }
}

#[cfg(test)]
mod witness_tests {
    use super::*;
    #[test]
    fn function_flow_witness_lookup_is_signature_indexed_and_bounded() {
        for count in [32, 128, 512, 2048] {
            let mut witnesses = TypeWitnesses::default();
            for value in 0..count {
                witnesses.add(
                    2,
                    Some(&format!("type-{value}")),
                    Span::new(value, value + 1, 1, 1),
                );
            }
            let mut wanted = TypeReach::default();
            wanted.add(2, Some("different"));
            assert!(witnesses.matching(&wanted, 2).is_none());
            wanted.add(2, Some(&format!("type-{}", count - 1)));
            assert_eq!(witnesses.matching(&wanted, 2).unwrap().start, count - 1);
            let probes = witnesses.probes.get();
            assert!(probes <= 3);
            println!("flow witness values={count} two_query_probes={probes}");
            let mut imported = TypeWitnesses::default();
            imported.add(4, None, Span::new(9000, 9001, 1, 1));
            witnesses.merge(imported);
            let mut wildcard = TypeReach::default();
            wildcard.add(4, Some("unknown-compatible"));
            assert_eq!(witnesses.matching(&wildcard, 4).unwrap().start, 9000);
            wildcard.add(2, None);
            assert!(witnesses.matching(&wildcard, 2).is_some());
            assert!(witnesses.matching(&wildcard, 8).is_none());
        }
    }
}
