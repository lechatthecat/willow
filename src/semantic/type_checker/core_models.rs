//! Executable finite models; definitions and exact bounds: formal/README.md.
//! Oracles use paths, rank certificates and permissions, not compiler analyses.
use super::TypeChecker;
use crate::diagnostics::{ErrorCode, Severity};
use crate::parser::ast::Type;
use crate::semantic::call_graph::{CallGraph, CallSites};
use crate::semantic::effects::{EffectProblem, RuntimeEffects};
use crate::semantic::ids::FunctionId;

fn check(source: &str, allowed: &[ErrorCode]) -> Vec<ErrorCode> {
    let tokens = crate::lexer::Lexer::new(source)
        .tokenize()
        .expect("model lex");
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{source}\n{errors:?}");
    let mut checker = TypeChecker::new();
    checker.set_enforce_send_sync(true);
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    let errors: Vec<_> = checker
        .errors
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert!(
        errors.iter().all(|d| allowed.contains(&d.code)),
        "{source}\n{errors:?}"
    );
    errors.iter().map(|d| d.code).collect()
}

fn graph(n: usize, bits: u64) -> Vec<Vec<usize>> {
    (0..n)
        .map(|i| (0..n).filter(|j| bits & (1 << (i * n + j)) != 0).collect())
        .collect()
}

// Small-step execution of calls: a state is the current callable. Each edge
// is a possible next call; cycles need only be visited once for reachability.
fn can_wait(edges: &[Vec<usize>], seeds: u64, transmit: u64, root: usize) -> bool {
    let mut seen = vec![false; edges.len()];
    let mut pending = vec![root];
    seen[root] = true;
    while let Some(node) = pending.pop() {
        if seeds & (1 << node) != 0 {
            return true;
        }
        for &next in &edges[node] {
            if transmit & (1 << next) != 0 && !std::mem::replace(&mut seen[next], true) {
                pending.push(next);
            }
        }
    }
    false
}

fn compare_effects(edges: &[Vec<usize>], seeds: u64, transmit: u64, effect: RuntimeEffects) {
    let id = |i| FunctionId::free(format!("model{i}"));
    let mut calls = CallGraph::default();
    let mut problem = EffectProblem::<usize>::new();
    for (i, targets) in edges.iter().enumerate() {
        calls.merge(
            id(i),
            CallSites {
                targets: targets.iter().map(|j| id(*j)).collect(),
                ..Default::default()
            },
        );
        problem = problem.body(id(i)).transmit(
            id(i),
            if transmit & (1 << i) != 0 {
                effect
            } else {
                RuntimeEffects::NONE
            },
        );
        if seeds & (1 << i) != 0 {
            problem = problem.seed(id(i), effect, Some(i));
        }
    }
    let facts = problem.solve(&calls);
    for root in 0..edges.len() {
        assert_eq!(
            facts.get(&id(root)).unwrap().effects().intersects(effect),
            can_wait(edges, seeds, transmit, root),
            "edges={edges:?} seeds={seeds} transmit={transmit} root={root}"
        );
    }
}

#[test]
fn core_model_effects_exhaustive_three_functions() {
    // All directed graphs (including recursion), local waits and call masks.
    for bits in 0..512 {
        let edges = graph(3, bits);
        for seeds in 0..8 {
            for transmit in 0..8 {
                compare_effects(&edges, seeds, transmit, RuntimeEffects::MAY_BLOCK);
            }
        }
    }
}

fn next(seed: &mut u64) -> u64 {
    // Fixed replayable corpus, no OS entropy or wall-clock dependent tests.
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

#[test]
fn core_model_effects_random_masks_and_source_differential() {
    let mut seed = 0x7361_6665_636f_7265;
    let mut accepted = 0;
    let mut rejected = 0;
    for case in 0..128 {
        let bits = next(&mut seed);
        let edges = graph(
            8,
            if case % 3 == 0 {
                bits & next(&mut seed) & next(&mut seed)
            } else {
                bits
            },
        );
        let waits = if case % 8 == 0 {
            0
        } else {
            next(&mut seed) & 255
        };
        compare_effects(
            &edges,
            waits,
            next(&mut seed) & 255,
            RuntimeEffects::MAY_SUSPEND,
        );
        let mut source = String::new();
        for (i, targets) in edges.iter().enumerate() {
            source.push_str(&format!("fn f{i}(ch: Channel<i64>) {{"));
            if waits & (1 << i) != 0 {
                source.push_str("ch.recv();");
            }
            for j in targets {
                source.push_str(&format!("f{j}(ch);"));
            }
            source.push('}');
        }
        source.push_str("async fn main() { let ch = Channel<i64>::new(); let m = Mutex::new(0); lock m as x { f0(ch); } }");
        let errors = check(&source, &[ErrorCode::E2604]);
        if errors.is_empty() {
            accepted += 1;
        } else {
            rejected += 1;
        }
        assert_eq!(
            !errors.is_empty(),
            can_wait(&edges, waits, 255, 0),
            "case={case}\n{source}"
        );
    }
    assert!(
        accepted >= 16 && rejected >= 16,
        "accepted={accepted}, rejected={rejected}"
    );
}

#[test]
fn core_model_missing_effect_evidence_is_top() {
    for mode in 0..3 {
        let root = FunctionId::free("root");
        let absent = FunctionId::free("absent");
        let mut calls = CallGraph::default();
        if mode != 0 {
            calls.merge(
                root,
                CallSites {
                    has_unknown: mode == 1,
                    targets: if mode == 2 {
                        [absent].into()
                    } else {
                        Default::default()
                    },
                    ..Default::default()
                },
            );
        }
        let facts = EffectProblem::<usize>::new().body(root).solve(&calls);
        let effects = facts.get(&root).unwrap().effects();
        assert!(effects.contains(RuntimeEffects::MAY_BLOCK.union(RuntimeEffects::MAY_SUSPEND)));
    }
}

// A proof certificate is a bijective rank assignment. This does not use SCCs,
// DFS cycle detection or the compiler's lock identities.
fn has_rank(edges: &[Vec<usize>]) -> bool {
    fn assign(edges: &[Vec<usize>], order: &mut Vec<usize>) -> bool {
        if order.len() == edges.len() {
            let mut rank = vec![0; order.len()];
            for (i, node) in order.iter().enumerate() {
                rank[*node] = i;
            }
            return edges
                .iter()
                .enumerate()
                .all(|(from, targets)| targets.iter().all(|to| rank[from] < rank[*to]));
        }
        for node in 0..edges.len() {
            if !order.contains(&node) {
                order.push(node);
                if assign(edges, order) {
                    return true;
                }
                order.pop();
            }
        }
        false
    }
    assign(edges, &mut Vec::new())
}

#[test]
fn core_model_lock_order_exhaustive_source_differential() {
    for bits in 0..512 {
        let edges = graph(3, bits);
        let mut source = "async fn main() { let m0 = Mutex::new(0); let m1 = Mutex::new(1); let m2 = Mutex::new(2);".to_owned();
        for (i, targets) in edges.iter().enumerate() {
            for j in targets {
                source.push_str(&format!("lock m{i} as x {{ lock m{j} as y {{}} }}"));
            }
        }
        source.push('}');
        assert_eq!(
            check(&source, &[ErrorCode::E2605]).is_empty(),
            has_rank(&edges),
            "graph={bits}\n{source}"
        );
    }
}

#[test]
fn core_model_rank_prevents_wait_cycles() {
    let mut accepted = 0;
    let mut rejected_with_cycle = 0;
    for bits in 0..512 {
        let edges = graph(3, bits);
        // Task i holds lock i and either finishes (3) or asks for lock j.
        // All held subsets are represented: a finishing task breaks a cycle.
        let mut has_deadlock = false;
        for choices in 0..64 {
            let requests: Vec<_> = (0..3).map(|i| (choices >> (2 * i)) & 3).collect();
            if requests
                .iter()
                .enumerate()
                .any(|(i, j)| *j != 3 && !edges[i].contains(j))
            {
                continue;
            }
            for start in 0..3 {
                let mut seen = [false; 3];
                let mut task = start;
                while task != 3 {
                    if std::mem::replace(&mut seen[task], true) {
                        has_deadlock = true;
                        break;
                    }
                    task = requests[task];
                }
            }
        }
        if has_rank(&edges) {
            accepted += 1;
            assert!(!has_deadlock, "{bits}");
        } else {
            rejected_with_cycle += 1;
            assert!(has_deadlock, "{bits}");
        }
    }
    assert_eq!((accepted, rejected_with_cycle), (25, 487));
}

// Finite structural type calculus. Model pair = (transfer permission, shared
// permission). The translation to production Type does not compute permission.
#[derive(Clone, Debug)]
enum Value {
    Scalar,
    Closure,
    Wrap(usize, Box<Value>),
}

impl Value {
    fn permissions(&self) -> (bool, bool) {
        match self {
            Self::Scalar => (true, true),
            Self::Closure => (false, false),
            Self::Wrap(kind, inner) => {
                let (send, sync) = inner.permissions();
                match kind {
                    0 | 5 => (send, false),
                    1 | 6 => (send, sync),
                    2 | 4 => (send, send),
                    3 => (send && sync, send && sync),
                    _ => unreachable!(),
                }
            }
        }
    }
    fn ty(&self) -> Type {
        match self {
            Self::Scalar => Type::I64,
            Self::Closure => Type::Closure(vec![], Box::new(Type::Void)),
            Self::Wrap(kind, inner) => {
                let ty = inner.ty();
                if *kind == 0 {
                    Type::Array(Box::new(ty))
                } else {
                    Type::Generic(
                        [
                            "",
                            "Option",
                            "Mutex",
                            "RwLock",
                            "Channel",
                            "Task",
                            "FrozenArray",
                        ][*kind]
                            .into(),
                        vec![ty],
                    )
                }
            }
        }
    }
}

#[test]
fn core_model_structural_markers_exhaustive_depth_three() {
    let checker = TypeChecker::new();
    let mut layer = vec![Value::Scalar, Value::Closure];
    let mut count = 0;
    for depth in 0..=3 {
        for value in &layer {
            let ty = value.ty();
            assert_eq!(
                (checker.is_send(&ty), checker.is_sync(&ty)),
                value.permissions(),
                "{value:?}"
            );
            count += 1;
        }
        if depth < 3 {
            layer = layer
                .into_iter()
                .flat_map(|v| (0..7).map(move |kind| Value::Wrap(kind, Box::new(v.clone()))))
                .collect();
        }
    }
    assert_eq!(count, 800);
}

mod loans;
