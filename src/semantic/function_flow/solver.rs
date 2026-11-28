//! Delta propagation: at most LIMIT target events and one event per reason
//! per node. Edges replay existing events once when installed dynamically.
use crate::semantic::ids::FunctionId;
use std::collections::{HashMap, HashSet, VecDeque};

pub(super) const LIMIT: usize = 32;
const DEPTH: u16 = 64;
pub(super) const EXTERNAL: u8 = 1;
pub(super) const STORAGE: u8 = 2;
pub(super) const CAPACITY: u8 = 4;
pub(super) const TOO_DEEP: u8 = 8;
pub(super) const UNRESOLVED: u8 = 16;

#[derive(Default)]
pub(super) struct Node {
    pub callable: bool,
    pub storage: bool,
    pub reference: bool,
    pub targets: HashMap<FunctionId, u16>,
    pub unknown: u8,
    edges: Vec<usize>,
    edge_set: HashSet<usize>,
    calls: Vec<usize>,
}
#[derive(Clone)]
pub(super) struct Signature {
    pub params: Vec<usize>,
    pub references: Vec<bool>,
    pub result: usize,
}
#[derive(Clone)]
struct Call {
    args: Vec<usize>,
    result: usize,
}
#[derive(Clone, Copy)]
enum Event {
    Target(FunctionId, u16),
    Unknown(u8),
}

#[derive(Default)]
pub(super) struct Solver {
    pub nodes: Vec<Node>,
    pub signatures: HashMap<FunctionId, Signature>,
    calls: Vec<Call>,
    queue: VecDeque<(usize, Event)>,
    pub operations: usize,
    pub edge_attempts: usize,
    pub edges: usize,
    escapes: HashSet<usize>,
    escaped: HashSet<FunctionId>,
    opaque_escape: bool,
    storage_slots: Vec<usize>,
    storage_escaped: bool,
    #[cfg(test)]
    pub storage_visits: usize,
    #[cfg(test)]
    pub external_arg_visits: usize,
}
impl Solver {
    pub fn node(&mut self) -> usize {
        let id = self.nodes.len();
        self.nodes.push(Node::default());
        id
    }
    pub fn unknown(&mut self, node: usize, reason: u8) {
        let new = reason & !self.nodes[node].unknown;
        if new != 0 {
            self.nodes[node].unknown |= new;
            self.queue.push_back((node, Event::Unknown(new)));
            // An opaque object/array can expose callback storage independently
            // of targets on this value node (for example a callback parameter).
            if new & (EXTERNAL | STORAGE) != 0 && self.nodes[node].storage {
                self.invalidate_storage();
            }
        }
    }
    pub fn target(&mut self, node: usize, target: FunctionId, depth: u16) {
        self.nodes[node].callable = true;
        if self.nodes[node].targets.contains_key(&target) {
            return;
        }
        if depth > DEPTH {
            self.unknown(node, TOO_DEEP);
            return;
        }
        if self.nodes[node].targets.len() == LIMIT {
            self.unknown(node, CAPACITY);
            return;
        }
        self.nodes[node].targets.insert(target, depth);
        self.queue.push_back((node, Event::Target(target, depth)));
    }
    fn send(&mut self, node: usize, event: Event) {
        self.operations += 1;
        match event {
            Event::Target(target, depth) => self.target(node, target, depth + 1),
            Event::Unknown(reason) => self.unknown(node, reason),
        }
    }
    pub fn edge(&mut self, from: usize, to: usize) {
        self.edge_attempts += 1;
        if from == to || !self.nodes[from].edge_set.insert(to) {
            return;
        }
        self.edges += 1;
        self.nodes[from].edges.push(to);
        let mut events: Vec<_> = self.nodes[from]
            .targets
            .iter()
            .map(|(&t, &d)| Event::Target(t, d))
            .collect();
        events.sort_by_key(|e| match e {
            Event::Target(id, _) => *id,
            Event::Unknown(_) => unreachable!(),
        });
        for event in events {
            self.send(to, event);
        }
        self.unknown(to, self.nodes[from].unknown);
    }
    pub fn register_signature(&mut self, target: FunctionId, signature: Signature) {
        // Eager constraint replay can discover an escape before a later lambda
        // or an overloaded signature has allocated all of its parameter nodes.
        if self.opaque_escape || self.escaped.contains(&target) {
            for &param in &signature.params {
                self.unknown(param, EXTERNAL);
            }
        }
        self.signatures.insert(target, signature);
    }
    pub fn invoke(&mut self, target: FunctionId, args: &[usize], result: usize) {
        if let Some(sig) = self.signatures.get(&target).cloned() {
            for (i, (&from, to)) in args.iter().zip(sig.params).enumerate() {
                self.edge(from, to);
                if sig.references.get(i) == Some(&true) {
                    self.edge(to, from);
                }
            }
            self.edge(sig.result, result);
        } else {
            self.external_call(args, result);
        }
    }
    pub fn escape(&mut self, node: usize) {
        if !self.escapes.insert(node) {
            return;
        }
        let mut targets: Vec<_> = self.nodes[node].targets.keys().copied().collect();
        targets.sort_unstable();
        for target in targets {
            self.escape_target(target);
        }
        if self.nodes[node].unknown != 0 {
            self.escape_unknown();
        }
    }
    /// Called once for each interned heap slot, including slots created after
    /// an eagerly resolved external call. Invalidation visits each slot once.
    pub fn storage_slot(&mut self, node: usize) {
        self.storage_slots.push(node);
        if self.storage_escaped {
            self.invalidate_slot(node);
        }
    }
    fn invalidate_slot(&mut self, node: usize) {
        #[cfg(test)]
        {
            self.storage_visits += 1;
        }
        self.unknown(node, STORAGE);
        self.escape(node);
    }
    pub fn invalidate_storage(&mut self) {
        if self.storage_escaped {
            return;
        }
        self.storage_escaped = true;
        for index in 0..self.storage_slots.len() {
            self.invalidate_slot(self.storage_slots[index]);
        }
    }
    /// One policy for direct, dynamically resolved and unknown external calls.
    /// Storage can escape through either actuals or results; references can
    /// replace their actual value as well as mutate the value's referent.
    pub fn external_call(&mut self, args: &[usize], result: usize) {
        self.unknown(result, EXTERNAL);
        if self.nodes[result].storage {
            self.invalidate_storage();
        }
        for &arg in args {
            #[cfg(test)]
            {
                self.external_arg_visits += 1;
            }
            if self.nodes[arg].storage {
                self.invalidate_storage();
            }
            if self.nodes[arg].reference {
                self.unknown(arg, EXTERNAL);
            }
            if self.nodes[arg].callable {
                self.escape(arg);
            }
        }
    }
    fn escape_target(&mut self, target: FunctionId) {
        if self.escaped.insert(target)
            && let Some(sig) = self.signatures.get(&target).cloned()
        {
            for param in sig.params {
                self.unknown(param, EXTERNAL);
            }
        }
    }
    fn escape_unknown(&mut self) {
        if self.opaque_escape {
            return;
        }
        self.opaque_escape = true;
        let mut params = vec![false; self.nodes.len()];
        for sig in self.signatures.values() {
            for &param in &sig.params {
                params[param] = true;
            }
        }
        for (param, used) in params.into_iter().enumerate() {
            if used {
                self.unknown(param, EXTERNAL);
            }
        }
    }
    pub fn call(&mut self, callee: usize, args: Vec<usize>, result: usize) {
        let id = self.calls.len();
        self.calls.push(Call { args, result });
        self.nodes[callee].calls.push(id);
        let mut targets: Vec<_> = self.nodes[callee].targets.keys().copied().collect();
        targets.sort_unstable();
        for target in targets {
            let call = self.calls[id].clone();
            self.invoke(target, &call.args, result);
        }
        if self.nodes[callee].unknown != 0 {
            self.external_call(&self.calls[id].args.clone(), result);
        }
    }
    pub fn solve(&mut self) {
        while let Some((node, event)) = self.queue.pop_front() {
            if self.escapes.contains(&node) {
                match event {
                    Event::Target(target, _) => self.escape_target(target),
                    Event::Unknown(_) => self.escape_unknown(),
                }
            }
            let edges: Vec<_> = self.nodes[node].edges.to_vec();
            for next in edges {
                self.send(next, event);
            }
            let calls = self.nodes[node].calls.clone();
            for id in calls {
                self.operations += 1;
                let call = self.calls[id].clone();
                match event {
                    Event::Target(target, _) => self.invoke(target, &call.args, call.result),
                    Event::Unknown(_) => {
                        self.external_call(&call.args, call.result);
                    }
                }
            }
        }
    }
}

pub(super) fn reasons(mask: u8) -> Vec<String> {
    [
        (EXTERNAL, "function-flow-external-escape"),
        (STORAGE, "function-flow-unknown-storage"),
        (CAPACITY, "function-flow-candidate-limit"),
        (TOO_DEEP, "function-flow-depth-limit"),
        (UNRESOLVED, "function-flow-unresolved-source"),
    ]
    .into_iter()
    .filter(|(bit, _)| mask & bit != 0)
    .map(|(_, name)| name.to_owned())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn function_flow_bounded_operations() {
        for n in [32, 128, 512, 2048] {
            for shape in ["chain", "fan", "cycle", "repeated"] {
                let mut s = Solver::default();
                for _ in 0..n {
                    s.node();
                }
                for i in 0..n - 1 {
                    match shape {
                        "fan" => {
                            s.edge(i, n - 1);
                            s.edge(n - 1, i);
                        }
                        "repeated" => {
                            for _ in 0..16 {
                                s.edge(i, i + 1);
                            }
                        }
                        _ => s.edge(i, i + 1),
                    }
                }
                if shape == "cycle" {
                    s.edge(n - 1, 0);
                }
                for i in 0..n {
                    s.target(
                        i,
                        FunctionId::free(format!(
                            "value{}",
                            if shape == "repeated" { i % 40 } else { i }
                        )),
                        0,
                    );
                }
                s.solve();
                assert!(s.nodes.iter().all(|node| node.targets.len() <= LIMIT));
                assert!(
                    s.operations <= 2 * (LIMIT + 5) * s.edges,
                    "{shape} n={n} {}",
                    s.operations
                );
                assert!(s.edge_attempts <= 16 * s.edges);
                println!(
                    "flow {shape} values={n} nodes={} edges={} propagation_operations={} edge_attempts={}",
                    s.nodes.len(),
                    s.edges,
                    s.operations,
                    s.edge_attempts
                );
            }
        }
    }
    #[test]
    fn function_flow_limits_fail_closed() {
        let mut s = Solver::default();
        let node = s.node();
        for i in 0..LIMIT + 1 {
            s.target(node, FunctionId::free(format!("f{i}")), 0);
        }
        assert_eq!(s.nodes[node].unknown, CAPACITY);
        assert_eq!(
            reasons(s.nodes[node].unknown),
            ["function-flow-candidate-limit"]
        );
        let mut s = Solver::default();
        for _ in 0..100 {
            s.node();
        }
        for i in 0..99 {
            s.edge(i, i + 1);
        }
        s.target(0, FunctionId::free("f"), 0);
        s.solve();
        assert_eq!(s.nodes[99].unknown, TOO_DEEP);
        assert!(s.nodes[99].targets.is_empty());
    }
    #[test]
    fn function_flow_dynamic_edges_and_external_inputs() {
        let mut s = Solver::default();
        let callee = s.node();
        let argument = s.node();
        let result = s.node();
        let param = s.node();
        let returned = s.node();
        let target = FunctionId::free("identity");
        s.signatures.insert(
            target,
            Signature {
                params: vec![param],
                references: vec![false],
                result: returned,
            },
        );
        s.edge(param, returned);
        s.call(callee, vec![argument], result);
        s.target(callee, target, 0);
        s.target(argument, FunctionId::free("safe"), 0);
        s.solve();
        assert!(
            s.nodes[result]
                .targets
                .contains_key(&FunctionId::free("safe"))
        );
        s.escape(callee);
        s.solve();
        assert_eq!(s.nodes[result].unknown, EXTERNAL);
    }
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    #[test]
    fn function_flow_external_storage_invalidation_is_one_pass() {
        for count in [32, 128, 512, 2048] {
            for mode in ["eager", "late-target", "unknown"] {
                let mut solver = Solver::default();
                let callee = solver.node();
                let argument = solver.node();
                solver.nodes[argument].storage = true;
                let result = solver.node();
                let mut slots = Vec::new();
                for _ in 0..count / 2 {
                    let field = solver.node();
                    solver.storage_slot(field);
                    slots.push(field);
                }
                if mode == "eager" {
                    solver.target(callee, FunctionId::free("external"), 0);
                }
                for _ in 0..count {
                    solver.call(callee, vec![argument], result);
                }
                if mode == "late-target" {
                    solver.target(callee, FunctionId::free("external"), 0);
                }
                if mode == "unknown" {
                    solver.unknown(callee, EXTERNAL);
                }
                solver.solve();
                for _ in count / 2..count {
                    let field = solver.node();
                    solver.storage_slot(field);
                    slots.push(field);
                }
                solver.solve();
                assert!(
                    slots
                        .iter()
                        .all(|&node| solver.nodes[node].unknown & STORAGE != 0)
                );
                assert_eq!(solver.storage_visits, count);
                assert!(solver.external_arg_visits <= 2 * count);
                assert!(solver.operations <= 2 * count);
                println!(
                    "external-storage mode={mode} fields={count} calls={count} slot_visits={} argument_visits={} operations={}",
                    solver.storage_visits, solver.external_arg_visits, solver.operations
                );
            }
        }
    }
    #[test]
    fn function_flow_late_signatures_retain_external_escape() {
        for opaque in [false, true] {
            let mut solver = Solver::default();
            let callback = solver.node();
            let target = FunctionId::free("late-callback");
            if opaque {
                solver.unknown(callback, EXTERNAL);
            } else {
                solver.target(callback, target, 0);
            }
            solver.escape(callback);
            let param = solver.node();
            let result = solver.node();
            solver.register_signature(
                target,
                Signature {
                    params: vec![param],
                    references: vec![false],
                    result,
                },
            );
            assert_ne!(solver.nodes[param].unknown & EXTERNAL, 0);
        }
    }
    #[test]
    fn function_flow_external_results_and_references_escape() {
        let mut solver = Solver::default();
        let slot = solver.node();
        solver.storage_slot(slot);
        let returned = solver.node();
        solver.nodes[returned].storage = true;
        solver.external_call(&[], returned);
        assert_ne!(solver.nodes[slot].unknown & STORAGE, 0);
        let actual = solver.node();
        solver.nodes[actual].reference = true;
        let binding = solver.node();
        solver.edge(actual, binding);
        let result = solver.node();
        solver.external_call(&[actual], result);
        solver.solve();
        assert_ne!(solver.nodes[binding].unknown & EXTERNAL, 0);
        assert_eq!(solver.storage_visits, 1);
    }
}
