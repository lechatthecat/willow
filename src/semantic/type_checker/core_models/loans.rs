//! Independent bounded loan semantics and generated source comparisons.
use super::{check, next};
use crate::diagnostics::ErrorCode;
use std::collections::HashSet;

const ALL: u8 = 0b111;
const NAMES: [&str; 3] = ["a", "b", "c"];
const PRELUDE: &str = "import std::collections::Array;
    async fn edit(x: Array<i64>) { x[0] = 2; }
    async fn editn(x: Array<Array<i64>>) { x[0][0] = 2; }
    fn consume(@sending x: Array<i64>) { x[0] = 2; }
    fn consumen(@sending x: Array<Array<i64>>) { x[0][0] = 2; }";

// Object identities and directed containment, not union-find components.
// No production ownership types, functions or interval logic are used here.
#[derive(Clone, Copy, Debug)]
struct Heap {
    variables: [usize; 3],
    contains: [u8; 3],
}

impl Heap {
    fn reachable(&self, variable: usize) -> u8 {
        let mut pending: u8 = 1 << self.variables[variable];
        let mut seen = 0;
        while pending != 0 {
            let object = pending.trailing_zeros() as usize;
            pending &= !(1 << object);
            seen |= 1 << object;
            pending |= self.contains[object] & !seen;
        }
        seen
    }

    fn footprints(&self) -> [u8; 3] {
        std::array::from_fn(|v| self.reachable(v))
    }
}

// Independent parent/child bits deliberately allow representing invalid
// states, so exclusivity is a checked invariant, not an enum tautology.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Permissions {
    parent: u8,
    children: [u8; 2],
}

impl Permissions {
    fn allocated() -> Self {
        Self {
            parent: ALL,
            children: [0; 2],
        }
    }

    fn parent_access(self, objects: u8) -> bool {
        self.parent & objects == objects
    }

    fn child_access(self, task: usize, objects: u8) -> bool {
        self.children[task] & objects == objects
    }

    fn lend(&mut self, task: usize, objects: u8) -> bool {
        if self.children[task] != 0 || !self.parent_access(objects) {
            return false;
        }
        self.parent &= !objects;
        self.children[task] = objects;
        true
    }

    fn join(&mut self, task: usize) {
        self.parent |= self.children[task];
        self.children[task] = 0;
    }

    fn transfer(&mut self, objects: u8) -> bool {
        if !self.parent_access(objects) {
            return false;
        }
        self.parent &= !objects;
        true
    }

    fn assert_exclusive(self) {
        assert_eq!(self.parent & (self.children[0] | self.children[1]), 0);
        assert_eq!(self.children[0] & self.children[1], 0);
    }
}

#[test]
fn core_model_permission_transfer_preserves_exclusivity() {
    let mut configurations = 0;
    let mut reached = 0;
    let mut denied_second = 0;
    let mut sizes = [0; 14];
    // All six possible non-self directed containment edges (cycles included),
    // and all maps of three variables onto three allocated objects.
    for edges in 0..64 {
        let mut contains = [0; 3];
        let mut bit = 0;
        for (from, row) in contains.iter_mut().enumerate() {
            for to in 0..3 {
                if from != to {
                    if edges & (1 << bit) != 0 {
                        *row |= 1 << to;
                    }
                    bit += 1;
                }
            }
        }
        for mapping in 0..27 {
            let heap = Heap {
                variables: [mapping % 3, mapping / 3 % 3, mapping / 9],
                contains,
            };
            let footprints = heap.footprints();
            let initial = Permissions::allocated();
            let mut seen = HashSet::from([initial]);
            let mut states = vec![initial];
            let mut index = 0;
            while index < states.len() {
                let state = states[index];
                state.assert_exclusive();
                assert_eq!(state.parent | state.children[0] | state.children[1], ALL);
                for objects in footprints {
                    for task in 0..2 {
                        let mut lent = state;
                        let permitted = lent.lend(task, objects);
                        assert_eq!(
                            permitted,
                            state.children[task] == 0 && state.parent_access(objects)
                        );
                        if permitted {
                            assert!(lent.child_access(task, objects));
                            assert!(!lent.parent_access(objects));
                            let mut second = lent;
                            assert!(!second.lend(1 - task, objects));
                            assert_eq!(second, lent);
                            denied_second += 1;
                            if seen.insert(lent) {
                                states.push(lent);
                            }
                        } else {
                            assert_eq!(lent, state);
                        }
                        // Reads/writes are stuttering transitions with separate
                        // guards. No other task may access a child's footprint.
                        if state.child_access(task, objects) {
                            assert!(!state.parent_access(objects));
                            assert!(!state.child_access(1 - task, objects));
                        }
                    }
                }
                for task in 0..2 {
                    let mut joined = state;
                    joined.join(task);
                    assert!(joined.parent_access(state.children[task]));
                    assert_eq!(joined.children[1 - task], state.children[1 - task]);
                    if seen.insert(joined) {
                        states.push(joined);
                    }
                }
                index += 1;
            }
            configurations += 1;
            reached += states.len();
            sizes[states.len()] += 1;
        }
    }
    assert_eq!(configurations, 1728);
    assert_eq!((reached, denied_second), (7980, 11772));
    assert_eq!(sizes, [0, 0, 0, 750, 0, 666, 0, 252, 0, 18, 0, 36, 0, 6]);
}

// Ten source shapes: b aliases a or is fresh; c aliases a/b, is fresh,
// or contains a/b. All objects are allocated before any task starts.
fn bindings(alias_b: bool, shape_c: usize) -> (Heap, String, bool) {
    let b = if alias_b { 0 } else { 1 };
    let mut heap = Heap {
        variables: [0, b, 2],
        contains: [0; 3],
    };
    let c = match shape_c {
        0 => {
            heap.variables[2] = 0;
            "a"
        }
        1 => {
            heap.variables[2] = b;
            "b"
        }
        2 => "[3]",
        3 => {
            heap.contains[2] = 1;
            "[a]"
        }
        4 => {
            heap.contains[2] = 1 << b;
            "[b]"
        }
        _ => unreachable!(),
    };
    (
        heap,
        format!(
            "let a = [1]; let b = {}; let c = {c};",
            if alias_b { "a" } else { "[2]" }
        ),
        shape_c >= 3,
    )
}

fn access(variable: usize, nested_c: bool, write: bool) -> String {
    let index = if variable == 2 && nested_c {
        "[0][0]"
    } else {
        "[0]"
    };
    if write {
        format!("{}{index} = 3;", NAMES[variable])
    } else {
        format!("println({}{index});", NAMES[variable])
    }
}

fn call(variable: usize, nested_c: bool, sending: bool) -> String {
    let name = match (sending, variable == 2 && nested_c) {
        (false, false) => "edit",
        (false, true) => "editn",
        (true, false) => "consume",
        (true, true) => "consumen",
    };
    format!("{name}({})", NAMES[variable])
}

fn rejected(source: &str) -> bool {
    // finish_transfer_ownership emits E2402 for array loans and E2408
    // for permanent transfer. Named-class diagnostic variants aren't used.
    !check(source, &[ErrorCode::E2402, ErrorCode::E2408]).is_empty()
}

#[derive(Clone, Copy, Debug)]
struct Task {
    variable: usize,
    start: usize,
    join: usize,
}

fn trace(alias_b: bool, shape_c: usize, tasks: [Task; 2], accesses: &[(usize, bool)]) -> bool {
    let (heap, bindings, nested_c) = bindings(alias_b, shape_c);
    let footprints = heap.footprints();
    let mut state = Permissions::allocated();
    let mut conflict = false;
    let mut source = format!("{PRELUDE} async fn main() {{ {bindings}");
    for point in 0..=accesses.len() {
        // Tie order is explicit: task 0 start/join, task 1 start/join, access.
        for (t, task) in tasks.iter().enumerate() {
            assert!(task.start <= task.join && task.join <= accesses.len());
            if point == task.start {
                source.push_str(&format!(
                    "let t{t} = {};",
                    call(task.variable, nested_c, false)
                ));
                conflict |= !state.lend(t, footprints[task.variable]);
            }
            if point == task.join {
                source.push_str(&format!("await t{t};"));
                state.join(t);
            }
        }
        if let Some(&(variable, write)) = accesses.get(point) {
            source.push_str(&access(variable, nested_c, write));
            conflict |= !state.parent_access(footprints[variable]);
        }
        state.assert_exclusive();
    }
    source.push('}');
    assert_eq!(rejected(&source), conflict, "tasks={tasks:?}\n{source}");
    conflict
}

#[test]
fn core_model_multiple_loan_trace_differential() {
    let mut seed = 0x6c6f_616e_7365_6564;
    let mut counts = [0; 2];
    let intervals: Vec<_> = (0..=3)
        .flat_map(|start| (start..=3).map(move |join| (start, join)))
        .collect();
    // Exhaustive bindings, both loan variables and both interval placements.
    // Access choices are the larger dimension: deterministic xorshift samples.
    for alias_b in [false, true] {
        for shape_c in 0..5 {
            for first in 0..3 {
                for second in 0..3 {
                    for &(start, join) in &intervals {
                        for &(other_start, other_join) in &intervals {
                            let accesses: Vec<_> = (0..3)
                                .map(|_| (next(&mut seed) as usize % 3, next(&mut seed) & 1 != 0))
                                .collect();
                            let bad = trace(
                                alias_b,
                                shape_c,
                                [
                                    Task {
                                        variable: first,
                                        start,
                                        join,
                                    },
                                    Task {
                                        variable: second,
                                        start: other_start,
                                        join: other_join,
                                    },
                                ],
                                &accesses,
                            );
                            counts[usize::from(bad)] += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(counts.iter().sum::<usize>(), 9000);
    // Longer traces exercise more placements without a Cartesian explosion.
    for _ in 0..1024 {
        let alias_b = next(&mut seed) & 1 != 0;
        let shape_c = next(&mut seed) as usize % 5;
        let tasks = std::array::from_fn(|_| {
            let variable = next(&mut seed) as usize % 3;
            let start = next(&mut seed) as usize % 6;
            let join = start + next(&mut seed) as usize % (6 - start);
            Task {
                variable,
                start,
                join,
            }
        });
        let accesses: Vec<_> = (0..5)
            .map(|_| (next(&mut seed) as usize % 3, next(&mut seed) & 1 != 0))
            .collect();
        counts[usize::from(trace(alias_b, shape_c, tasks, &accesses))] += 1;
    }
    assert_eq!(counts, [2552, 7472]);
}

fn conservative(source: &str, conflict: bool, counts: &mut [usize; 3]) -> bool {
    let bad = rejected(source);
    assert!(!conflict || bad, "unsound acceptance:\n{source}");
    // accepted, correctly rejected, conservatively rejected
    counts[if !bad {
        0
    } else if conflict {
        1
    } else {
        2
    }] += 1;
    bad
}

#[test]
fn core_model_loan_branch_rebinding_soundness() {
    let mut counts = [0; 3];
    for alias_b in [false, true] {
        for conditional in [false, true] {
            for condition in [false, true] {
                for fresh in [false, true] {
                    for lent in 0..3 {
                        for used in 0..3 {
                            for joined in [false, true] {
                                let mut heap = Heap {
                                    variables: [0, usize::from(!alias_b), 2],
                                    contains: [0; 3],
                                };
                                let assignment = if fresh { "b = [2];" } else { "b = a;" };
                                let assignment = if conditional {
                                    format!("if cond {{ {assignment} }}")
                                } else {
                                    assignment.to_owned()
                                };
                                if !conditional || condition {
                                    heap.variables[1] = usize::from(fresh);
                                }
                                let footprints = heap.footprints();
                                let mut state = Permissions::allocated();
                                assert!(state.lend(0, footprints[lent]));
                                if joined {
                                    state.join(0);
                                }
                                let conflict = !state.parent_access(footprints[used]);
                                let source = format!(
                                    "{PRELUDE} async fn main() {{ let cond = {condition}; let a = [1]; let mut b = {}; let c = [3]; {assignment} let t = {}; {} {} {} }}",
                                    if alias_b { "a" } else { "[2]" },
                                    call(lent, false, false),
                                    if joined { "await t;" } else { "" },
                                    access(used, false, false),
                                    if joined { "" } else { "await t;" }
                                );
                                conservative(&source, conflict, &mut counts);
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(counts, [216, 64, 8]);
}

#[test]
fn core_model_loan_loop_soundness() {
    let mut counts = [0; 3];
    for outer in [false, true] {
        for alias_b in [false, true] {
            for shape_c in 0..5 {
                for lent in 0..3 {
                    for used in 0..3 {
                        for joined in [false, true] {
                            let (heap, bindings, nested_c) = bindings(alias_b, shape_c);
                            let footprints = heap.footprints();
                            let mut conflict = false;
                            let mut state = Permissions::allocated();
                            for _ in 0..2 {
                                if !outer {
                                    state = Permissions::allocated();
                                }
                                conflict |= !state.lend(0, footprints[lent]);
                                if joined {
                                    state.join(0);
                                }
                                conflict |= !state.parent_access(footprints[used]);
                                if !joined {
                                    state.join(0);
                                }
                            }
                            let source = format!(
                                "{PRELUDE} async fn main() {{ {} for i in 0..2 {{ {} let t = {}; {} {} {} }} }}",
                                if outer { &bindings } else { "" },
                                if outer { "" } else { &bindings },
                                call(lent, nested_c, false),
                                if joined { "await t;" } else { "" },
                                access(used, nested_c, false),
                                if joined { "" } else { "await t;" }
                            );
                            let bad = conservative(&source, conflict, &mut counts);
                            // Pin the language's iteration-origin restriction,
                            // including safe finite executions it overrejects.
                            assert_eq!(bad, outer || conflict, "{source}");
                        }
                    }
                }
            }
        }
    }
    assert_eq!(counts, [116, 128, 116]);
}

#[test]
fn core_model_loan_sending_soundness() {
    let mut counts = [0; 3];
    for alias_b in [false, true] {
        for shape_c in 0..5 {
            for sent in 0..3 {
                for used in 0..3 {
                    for before in [false, true] {
                        for use_as_loan in [false, true] {
                            let (heap, bindings, nested_c) = bindings(alias_b, shape_c);
                            let footprints = heap.footprints();
                            let mut state = Permissions::allocated();
                            if !before {
                                assert!(state.transfer(footprints[sent]));
                            }
                            let conflict = if use_as_loan {
                                let bad = !state.lend(0, footprints[used]);
                                state.join(0);
                                bad
                            } else {
                                !state.parent_access(footprints[used])
                            };
                            if before {
                                assert!(state.transfer(footprints[sent]));
                            }
                            let transfer = format!("{};", call(sent, nested_c, true));
                            let usage = if use_as_loan {
                                format!("await {};", call(used, nested_c, false))
                            } else {
                                access(used, nested_c, false)
                            };
                            let source = format!(
                                "{PRELUDE} async fn main() {{ {bindings} {} {usage} {} }}",
                                if before { "" } else { &transfer },
                                if before { &transfer } else { "" }
                            );
                            conservative(&source, conflict, &mut counts);
                        }
                    }
                }
            }
        }
    }
    assert_eq!(counts, [232, 128, 0]);
}

#[test]
fn core_model_exclusive_loan_trace_differential() {
    // One fresh heap object, two aliases, one child. Enumerate every ordering
    // of borrow/join and four parent accesses (before, during, after; direct or
    // alias). The child's write permission overlaps the parent's iff rejected.
    let mut cases = 0;
    for start in 0..=4 {
        for join in start..=4 {
            for aliases in 0..16 {
                let mut source = "import std::collections::Array; async fn edit(a: Array<i64>) { a[0] = 2; } async fn main() { let a = [1]; let b = a;".to_owned();
                let mut child_owns = false;
                let mut conflict = false;
                for point in 0..=4 {
                    if point == start {
                        source.push_str("let t = edit(a);");
                        child_owns = true;
                    }
                    if point == join {
                        source.push_str("await t;");
                        child_owns = false;
                    }
                    if point < 4 {
                        source.push_str(if aliases & (1 << point) == 0 {
                            "println(a[0]);"
                        } else {
                            "println(b[0]);"
                        });
                        conflict |= child_owns;
                    }
                }
                source.push('}');
                assert_eq!(
                    !check(&source, &[ErrorCode::E2402, ErrorCode::E2408]).is_empty(),
                    conflict,
                    "{source}"
                );
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 240);
}
