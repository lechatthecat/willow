//! Fuse string concatenation chains into one sized allocation
//! (willow-8hq4.29).
//!
//! `"k" + i.toString() + ":" + j.toString()` lowers to a left-nested chain of
//! two-operand `String` additions whose every intermediate result is a fresh
//! temporary read exactly once. Evaluated pairwise, a chain of `n` parts
//! allocates `n - 1` strings and copies `O(n * total_len)` bytes. This pass
//! rewrites the chain into one [`LirRvalue::StringConcat`], which the runtime
//! sizes first and fills once: one allocation and `O(total_len)` copying.
//! `i64.toString()` and `{i64}` interpolation operands become decimal parts
//! that are formatted straight into the result, never allocated on their own.
//!
//! Only the order of the intermediate allocations changes. Operands are still
//! evaluated in source order by the instructions that compute them; a fused
//! part only defers the READ of its operand local to the consuming
//! instruction, so the read is legal only while nothing writes that local in
//! between. Heap places are never deferred.
use std::collections::HashMap;

use crate::ir::lowered::async_liveness::{instruction_use_def, terminator_uses};
use crate::ir::lowered::{
    LirLocal, LirLocalId, LirOperand, LirRvalue, SourceBlock, SourceFunction, SourceInst,
};
use crate::parser::ast::BinOp;
use crate::semantic::ids::SemanticType as Type;
use crate::semantic::intrinsics::Intrinsic;

/// Fuse the concatenation chains of `function` and of its defer bodies.
pub(crate) fn fuse_string_concats(function: &mut SourceFunction) {
    let mut functions = vec![function];
    while let Some(function) = functions.pop() {
        fuse_function(function);
        for block in &mut function.blocks {
            for instruction in &mut block.instrs {
                if let SourceInst::Defer { body, .. } = instruction {
                    functions.push(body.function.as_mut());
                }
            }
        }
    }
}

/// How often each local is read and written, counted per instruction.
struct Accesses {
    reads: Vec<u32>,
    writes: Vec<u32>,
}

impl Accesses {
    fn count(function: &SourceFunction) -> Self {
        let names: HashMap<_, _> = function
            .locals
            .iter()
            .map(|local| (local.name.as_str(), local.id))
            .collect();
        let mut reads = vec![0u32; function.locals.len()];
        let mut writes = vec![0u32; function.locals.len()];
        let mut uses = std::collections::HashSet::new();
        let mut defs = std::collections::HashSet::new();
        for block in &function.blocks {
            for instruction in &block.instrs {
                uses.clear();
                defs.clear();
                instruction_use_def(instruction, &names, &mut uses, &mut defs);
                // A Compute reads its operands before writing its result, so a
                // self-update `t = t + x` counts as both.
                if let SourceInst::Compute { local, value, .. } = instruction
                    && value
                        .operands()
                        .iter()
                        .any(|op| op.locals().contains(local))
                {
                    uses.insert(*local);
                }
                for id in &uses {
                    reads[id.0 as usize] += 1;
                }
                for id in &defs {
                    writes[id.0 as usize] += 1;
                }
            }
            uses.clear();
            terminator_uses(&block.terminator, &names, &mut uses, &defs);
            for id in &uses {
                reads[id.0 as usize] += 1;
            }
        }
        Self { reads, writes }
    }
}

/// One operand of a fused chain: a leaf value, or a whole absorbed node.
enum Part {
    String(LirOperand),
    I64(LirOperand),
    Node(usize),
}

struct Node {
    /// Instruction index of the defining `Compute` within its block.
    index: usize,
    local: LirLocalId,
    parts: Vec<Part>,
    absorbed_any: bool,
    /// The node this one was absorbed into, if any.
    parent: Option<usize>,
}

/// A concatenation-shaped right-hand side: its parts, unexpanded. A `String`
/// copy is a one-part chain, so a chain can flow through it.
fn concat_parts(value: &LirRvalue, string_result: bool) -> Option<Vec<Part>> {
    let leaf = |op: &LirOperand, ty: &Type| match ty {
        Type::String => Some(Part::String(op.clone())),
        Type::I64 => Some(Part::I64(op.clone())),
        _ => None,
    };
    let parts = match value {
        LirRvalue::Binary {
            op: BinOp::Add,
            lhs,
            rhs,
            operand_ty: Type::String,
        } => vec![Part::String(lhs.clone()), Part::String(rhs.clone())],
        LirRvalue::StringConcat { parts, part_types } => parts
            .iter()
            .zip(part_types)
            .map(|(op, ty)| leaf(op, ty))
            .collect::<Option<_>>()?,
        LirRvalue::FormatScalar {
            value,
            ty,
            format: None,
        } => vec![leaf(value, ty)?],
        LirRvalue::IntrinsicCall {
            intrinsic: Intrinsic::I64ToString,
            receiver,
            ..
        } => vec![Part::I64(receiver.clone())],
        LirRvalue::Use(op) if string_result => vec![Part::String(op.clone())],
        _ => return None,
    };
    // Deferring a heap read past intervening instructions is unsound.
    let deferrable = parts.iter().all(|part| match part {
        Part::String(op) | Part::I64(op) => !matches!(op, LirOperand::Reference { .. }),
        Part::Node(_) => true,
    });
    deferrable.then_some(parts)
}

fn root(nodes: &mut [Node], mut node: usize) -> usize {
    let mut top = node;
    while let Some(parent) = nodes[top].parent {
        top = parent;
    }
    // Path compression keeps repeated leaf invalidation near-linear.
    while let Some(parent) = nodes[node].parent {
        nodes[node].parent = Some(top);
        node = parent;
    }
    top
}

fn fuse_function(function: &mut SourceFunction) {
    let accesses = Accesses::count(function);
    for block in &mut function.blocks {
        fuse_block(block, &accesses, &function.locals);
    }
}

fn fuse_block(block: &mut SourceBlock, accesses: &Accesses, locals: &[LirLocal]) {
    let string = |id: LirLocalId| locals[id.0 as usize].ty == Type::String;
    // Only a compiler temporary may vanish into its consumer.
    let fusible = |id: LirLocalId| {
        let local = &locals[id.0 as usize];
        local.synthetic && !local.parameter && local.ty == Type::String
    };
    let mut nodes: Vec<Node> = Vec::new();
    // The not-yet-consumed concatenation node each local currently holds,
    // tagged with the barrier epoch it was defined in.
    let mut pending: HashMap<LirLocalId, (usize, u32)> = HashMap::new();
    // Leaf locals whose reads some node defers, for invalidation on write.
    let mut leaf_readers: HashMap<LirLocalId, Vec<usize>> = HashMap::new();
    let mut epoch = 0u32;
    let mut operand_locals = Vec::new();
    for (index, instruction) in block.instrs.iter().enumerate() {
        let SourceInst::Compute { local, value, .. } = instruction else {
            // Anything else may read or write locals through expressions.
            epoch += 1;
            continue;
        };
        let local = *local;
        operand_locals.clear();
        for operand in value.operands() {
            operand_locals.extend(operand.locals());
        }
        let current = |pending: &HashMap<LirLocalId, (usize, u32)>, id: &LirLocalId| {
            pending
                .get(id)
                .filter(|(_, defined)| *defined == epoch)
                .map(|(node, _)| *node)
        };
        let mut node_parts = concat_parts(value, string(local));
        if let Some(parts) = &mut node_parts {
            for part in parts.iter_mut() {
                let Part::String(LirOperand::Local(id)) = part else {
                    continue;
                };
                let id = *id;
                let Some(child) = current(&pending, &id) else {
                    continue;
                };
                let read_once = operand_locals.iter().filter(|&&l| l == id).count() == 1;
                // The absorbed value must have no other reader: either this
                // instruction overwrites it, or it is a single-def, single-use
                // temporary.
                let sole_reader = local == id
                    || (accesses.reads[id.0 as usize] == 1 && accesses.writes[id.0 as usize] == 1);
                if read_once && sole_reader {
                    pending.remove(&id);
                    *part = Part::Node(child);
                }
            }
        }
        // Every other pending value this instruction reads escapes as is.
        for id in &operand_locals {
            pending.remove(id);
        }
        // Writing a local invalidates every deferred read of it.
        if let Some(readers) = leaf_readers.remove(&local) {
            for reader in readers {
                let top = root(&mut nodes, reader);
                if current(&pending, &nodes[top].local) == Some(top) {
                    pending.remove(&nodes[top].local);
                }
            }
        }
        pending.remove(&local);
        let Some(parts) = node_parts else {
            continue;
        };
        let id = nodes.len();
        let mut absorbed_any = false;
        for part in &parts {
            match part {
                Part::Node(child) => {
                    nodes[*child].parent = Some(id);
                    absorbed_any = true;
                }
                Part::String(op) | Part::I64(op) => {
                    for leaf in op.locals() {
                        leaf_readers.entry(leaf).or_default().push(id);
                    }
                }
            }
        }
        nodes.push(Node {
            index,
            local,
            parts,
            absorbed_any,
            parent: None,
        });
        if fusible(local) {
            pending.insert(local, (id, epoch));
        }
    }
    if !nodes.iter().any(|node| node.absorbed_any) {
        return;
    }
    let mut removed = vec![false; block.instrs.len()];
    let mut stack = Vec::new();
    for top in 0..nodes.len() {
        if nodes[top].parent.is_some() {
            removed[nodes[top].index] = true;
            continue;
        }
        if !nodes[top].absorbed_any {
            continue;
        }
        // Flatten in source order; every absorbed node is expanded once.
        let mut parts = Vec::new();
        let mut part_types = Vec::new();
        stack.clear();
        stack.extend(std::mem::take(&mut nodes[top].parts).into_iter().rev());
        while let Some(part) = stack.pop() {
            match part {
                Part::Node(child) => {
                    stack.extend(std::mem::take(&mut nodes[child].parts).into_iter().rev());
                }
                Part::String(op) => {
                    parts.push(op);
                    part_types.push(Type::String);
                }
                Part::I64(op) => {
                    parts.push(op);
                    part_types.push(Type::I64);
                }
            }
        }
        let fused = match (parts.len(), part_types.as_slice()) {
            (1, [Type::String]) => LirRvalue::Use(parts.pop().expect("one part")),
            (2, [Type::String, Type::String]) => {
                let rhs = parts.pop().expect("two parts");
                LirRvalue::Binary {
                    op: BinOp::Add,
                    lhs: parts.pop().expect("two parts"),
                    rhs,
                    operand_ty: Type::String,
                }
            }
            _ => LirRvalue::StringConcat { parts, part_types },
        };
        let SourceInst::Compute { value, .. } = &mut block.instrs[nodes[top].index] else {
            unreachable!("concatenation nodes are Compute instructions");
        };
        *value = fused;
    }
    let mut index = 0;
    block.instrs.retain(|_| {
        index += 1;
        !removed[index - 1]
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Span;
    use crate::ir::lowered::{
        BlockId, LirLocal, LirPlace, LirStorageKind, SourceProgram, SourceTerminator,
    };

    fn lowered(source: &str) -> SourceProgram {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (ast, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        checker.check_program(&ast);
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        let tables = crate::ir::lower::SemanticFacts::from_checker(&checker);
        let (hir, errors) = crate::ir::lower::lower_program_with(&ast, &tables);
        assert!(errors.is_empty(), "{errors:?}");
        crate::ir::lowered::lower_source_program(&hir)
    }

    fn function<'a>(program: &'a SourceProgram, name: &str) -> &'a SourceFunction {
        program
            .functions
            .iter()
            .find(|f| f.name.is_free_named(name))
            .unwrap()
    }

    /// Every Compute right-hand side of `function` and its defer bodies.
    fn values(function: &SourceFunction) -> Vec<&LirRvalue> {
        let mut values = Vec::new();
        let mut functions = vec![function];
        while let Some(function) = functions.pop() {
            for instruction in function.blocks.iter().flat_map(|block| &block.instrs) {
                match instruction {
                    SourceInst::Compute { value, .. } => values.push(value),
                    SourceInst::Defer { body, .. } => functions.push(&body.function),
                    _ => {}
                }
            }
        }
        values
    }

    /// Part types of each fused concatenation.
    fn concats(function: &SourceFunction) -> Vec<Vec<Type>> {
        values(function)
            .into_iter()
            .filter_map(|value| match value {
                LirRvalue::StringConcat { part_types, .. } => Some(part_types.clone()),
                _ => None,
            })
            .collect()
    }

    fn string_adds(function: &SourceFunction) -> usize {
        values(function)
            .into_iter()
            .filter(|value| {
                matches!(
                    value,
                    LirRvalue::Binary {
                        op: BinOp::Add,
                        operand_ty: Type::String,
                        ..
                    }
                )
            })
            .count()
    }

    fn i64_to_strings(function: &SourceFunction) -> usize {
        values(function)
            .into_iter()
            .filter(|value| {
                matches!(
                    value,
                    LirRvalue::IntrinsicCall {
                        intrinsic: Intrinsic::I64ToString,
                        ..
                    }
                )
            })
            .count()
    }

    use crate::parser::ast::Type::{I64, String as S};

    #[test]
    fn left_nested_chain_becomes_one_concatenation() {
        let program = lowered(
            r#"fn f(a: String, b: String) -> String { return a + "-" + b + "!"; }
               fn main() { println(f("x", "y")); }"#,
        );
        let f = function(&program, "f");
        assert_eq!(concats(f), vec![vec![S, S, S, S]]);
        assert_eq!(string_adds(f), 0);
    }

    #[test]
    fn two_string_operands_keep_the_pairwise_entry() {
        let program = lowered(
            r#"fn f(a: String, b: String) -> String { return a + b; }
               fn main() { println(f("x", "y")); }"#,
        );
        let f = function(&program, "f");
        assert!(concats(f).is_empty());
        assert_eq!(string_adds(f), 1);
    }

    #[test]
    fn i64_to_string_operands_become_decimal_parts() {
        let program = lowered(
            r#"fn f(a: String, n: i64) -> String { return a + n.toString(); }
               fn g(n: i64) -> String { return "k" + n.toString() + ":" + (n * 7).toString(); }
               fn main() { println(f("x", 1)); println(g(2)); }"#,
        );
        assert_eq!(concats(function(&program, "f")), vec![vec![S, I64]]);
        assert_eq!(concats(function(&program, "g")), vec![vec![S, I64, S, I64]]);
        assert_eq!(i64_to_strings(function(&program, "g")), 0);
    }

    #[test]
    fn a_lone_to_string_is_left_alone() {
        let program = lowered(
            r#"fn f(n: i64) -> String { return n.toString(); }
               fn main() { println(f(1)); }"#,
        );
        let f = function(&program, "f");
        assert!(concats(f).is_empty());
        assert_eq!(i64_to_strings(f), 1);
    }

    #[test]
    fn right_and_balanced_nesting_flatten_in_source_order() {
        let program = lowered(
            r#"fn r(a: String, b: String, c: String) -> String { return a + (b + c); }
               fn q(a: String, b: String, c: String, n: i64) -> String {
                   return (a + n.toString()) + (b + c);
               }
               fn main() { println(r("a", "b", "c")); println(q("a", "b", "c", 1)); }"#,
        );
        assert_eq!(concats(function(&program, "r")), vec![vec![S, S, S]]);
        assert_eq!(concats(function(&program, "q")), vec![vec![S, I64, S, S]]);
        let q = function(&program, "q");
        let fused = values(q)
            .into_iter()
            .find_map(|value| match value {
                LirRvalue::StringConcat { parts, .. } => Some(parts.clone()),
                _ => None,
            })
            .unwrap();
        // Parameters are locals 0..4 in declaration order: `a`, decimal `n`,
        // `b`, `c`.
        assert_eq!(fused[0], LirOperand::Local(LirLocalId(0)));
        assert_eq!(fused[2], LirOperand::Local(LirLocalId(1)));
        assert_eq!(fused[3], LirOperand::Local(LirLocalId(2)));
    }

    #[test]
    fn format_fuses_integer_and_string_arguments() {
        let program = lowered(
            r#"fn f(name: String, n: i64) -> String { return format("{}:{}:{}!", name, n, 1.5); }
               fn main() { println(f("x", 1)); }"#,
        );
        let f = function(&program, "f");
        assert_eq!(concats(f), vec![vec![S, S, I64, S, S, S]]);
        // The f64 argument keeps its own formatting call.
        assert!(
            values(f)
                .iter()
                .any(|value| matches!(value, LirRvalue::FormatScalar { ty: Type::F64, .. }))
        );
    }

    #[test]
    fn calls_between_operands_still_fuse() {
        let program = lowered(
            r#"fn p(s: String) -> String { println(s); return s; }
               fn f() -> String { return p("a") + p("b") + p("c"); }
               fn main() { println(f()); }"#,
        );
        assert_eq!(concats(function(&program, "f")), vec![vec![S, S, S]]);
    }

    #[test]
    fn named_intermediates_are_not_absorbed() {
        let program = lowered(
            r#"fn f(a: String, b: String) -> String { let t = a + b; return t + a; }
               fn main() { println(f("x", "y")); }"#,
        );
        let f = function(&program, "f");
        assert!(concats(f).is_empty());
        assert_eq!(string_adds(f), 2);
    }

    #[test]
    fn defer_and_lambda_bodies_fuse() {
        let program = lowered(
            r#"fn f(a: String, n: i64) {
                   defer println(a + "-" + n.toString());
                   println("body");
               }
               fn main() {
                   let g = |s: String| s + "!" + "?";
                   println(g("x"));
                   f("y", 2);
               }"#,
        );
        assert_eq!(concats(function(&program, "f")), vec![vec![S, S, I64]]);
        let lambda = program
            .lambdas
            .iter()
            .find(|lambda| !concats(&lambda.function).is_empty())
            .expect("fused lambda body");
        assert_eq!(concats(&lambda.function), vec![vec![S, S, S]]);
    }

    // -- hand-built blocks ---------------------------------------------------

    /// `sources` named String locals, then `temps` synthetic String temporaries.
    fn hand_built(sources: u32, temps: u32, instrs: Vec<SourceInst>) -> SourceFunction {
        let mut function = lowered("fn main() {}")
            .functions
            .into_iter()
            .find(|f| f.name.is_free_named("main"))
            .unwrap();
        function.locals = (0..sources + temps)
            .map(|id| LirLocal {
                storage_kind: LirStorageKind::GcOwner,
                id: LirLocalId(id),
                name: format!("l{id}"),
                ty: Type::String,
                source_span: None,
                synthetic: id >= sources,
                parameter: false,
            })
            .collect();
        function.blocks = vec![SourceBlock {
            id: BlockId(0),
            instrs,
            terminator: SourceTerminator::Return(None),
            recovery: Vec::new(),
        }];
        function
    }

    fn local(id: u32) -> LirOperand {
        LirOperand::Local(LirLocalId(id))
    }

    fn compute(target: u32, value: LirRvalue) -> SourceInst {
        SourceInst::Compute {
            local: LirLocalId(target),
            value,
            span: Span::dummy(),
        }
    }

    fn add(target: u32, lhs: LirOperand, rhs: LirOperand) -> SourceInst {
        compute(
            target,
            LirRvalue::Binary {
                op: BinOp::Add,
                lhs,
                rhs,
                operand_ty: Type::String,
            },
        )
    }

    fn fused(instrs: Vec<SourceInst>, sources: u32, temps: u32) -> SourceFunction {
        let mut function = hand_built(sources, temps, instrs);
        fuse_string_concats(&mut function);
        function
    }

    #[test]
    fn a_temporary_read_twice_is_not_absorbed() {
        let f = fused(
            vec![
                add(3, local(0), local(1)),
                add(4, local(3), local(2)),
                add(5, local(3), local(0)),
            ],
            3,
            3,
        );
        assert!(concats(&f).is_empty());
        assert_eq!(f.blocks[0].instrs.len(), 3);
    }

    #[test]
    fn a_double_read_in_one_instruction_is_not_absorbed() {
        let f = fused(
            vec![add(3, local(0), local(1)), add(4, local(3), local(3))],
            3,
            2,
        );
        assert!(concats(&f).is_empty());
    }

    #[test]
    fn a_write_to_a_deferred_operand_blocks_absorption() {
        let f = fused(
            vec![
                add(3, local(0), local(1)),
                compute(0, LirRvalue::Use(local(2))),
                add(4, local(3), local(2)),
            ],
            3,
            2,
        );
        assert!(concats(&f).is_empty());
        assert_eq!(f.blocks[0].instrs.len(), 3);
        // A write to an unrelated local does not.
        let f = fused(
            vec![
                add(3, local(0), local(1)),
                compute(2, LirRvalue::Use(local(0))),
                add(4, local(3), local(2)),
            ],
            3,
            2,
        );
        assert_eq!(concats(&f), vec![vec![S, S, S]]);
        assert_eq!(f.blocks[0].instrs.len(), 2);
    }

    #[test]
    fn a_write_invalidates_the_whole_absorbed_tree() {
        // `l0` is a leaf of `t3`, which `t4` absorbed; overwriting `l0` before
        // `t5` must stop `t5` from deferring `t3`'s read of it.
        let f = fused(
            vec![
                add(3, local(0), local(1)),
                add(4, local(3), local(2)),
                compute(0, LirRvalue::Use(local(2))),
                add(5, local(4), local(1)),
            ],
            3,
            3,
        );
        assert_eq!(concats(&f), vec![vec![S, S, S]]);
        assert_eq!(string_adds(&f), 1);
    }

    #[test]
    fn non_compute_instructions_are_barriers() {
        let f = fused(
            vec![
                add(3, local(0), local(1)),
                SourceInst::ClearScopeRoots { locals: Vec::new() },
                add(4, local(3), local(2)),
            ],
            3,
            2,
        );
        assert!(concats(&f).is_empty());
    }

    #[test]
    fn heap_places_are_never_deferred() {
        let place = LirOperand::Reference {
            place: LirPlace::Local(LirLocalId(1)),
            span: Span::dummy(),
            display: "l1".into(),
        };
        let f = fused(
            vec![add(3, local(0), place), add(4, local(3), local(2))],
            3,
            2,
        );
        assert!(concats(&f).is_empty());
    }

    #[test]
    fn a_self_updating_temporary_chain_fuses() {
        let f = fused(
            vec![
                compute(3, LirRvalue::Use(local(0))),
                add(3, local(3), local(1)),
                add(3, local(3), local(2)),
                compute(4, LirRvalue::Use(local(3))),
            ],
            3,
            2,
        );
        assert_eq!(concats(&f), vec![vec![S, S, S]]);
        assert_eq!(f.blocks[0].instrs.len(), 2);
    }

    #[test]
    fn a_long_chain_is_flattened_without_recursion() {
        const PARTS: u32 = 100_000;
        let mut instrs = vec![add(2, local(0), local(1))];
        for temp in 3..PARTS + 1 {
            instrs.push(add(temp, local(temp - 1), local(0)));
        }
        let f = fused(instrs, 2, PARTS - 1);
        assert_eq!(f.blocks[0].instrs.len(), 1);
        let parts = concats(&f);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].len(), PARTS as usize);
    }
}
