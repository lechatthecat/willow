use super::*;
use crate::ir::lowered::{LirFunction, LirInst, LirOperand, LirPlace, LirRvalue};

fn program(source: &str) -> super::super::LirProgram {
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (ast, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let (hir, errors) = crate::ir::lower::lower_program(&ast);
    assert!(errors.is_empty(), "{errors:?}");
    super::super::lower_program(&hir)
}

fn main_function(source: &str) -> LirFunction {
    program(source).functions.pop().unwrap()
}

fn assert_stable(function: &LirFunction, name: &str) {
    let local = function
        .locals
        .iter()
        .find(|local| local.name == name)
        .unwrap();
    let slot = function.async_frame.slot(local.id).expect("promoted local");
    assert_eq!(function.async_frame.slots[slot.index], local.id);
    assert_eq!(
        function
            .async_frame
            .locals
            .values()
            .filter(|other| **other == slot)
            .count(),
        1
    );
    assert_eq!(stable_borrow_rejection(function), None);
}

#[test]
fn stable_borrow_scalar_storage_matrix() {
    // 24 explicit perspectives: three representations times eight CFG shapes.
    for (ty, literal) in [("i64", "42"), ("f64", "4.25"), ("bool", "true")] {
        for body in [
            format!("let x: {ty} = {literal}; read(&x);"),
            format!("let x: {ty} = {literal}; read(&x); await sleep(0);"),
            format!("let x: {ty} = {literal}; await sleep(0); read(&x);"),
            format!("let x: {ty} = {literal}; read(&x); await sleep(0); read(&x);"),
            format!("let x: {ty} = {literal}; if true {{ read(&x); }} else {{ await sleep(0); }}"),
            format!(
                "let x: {ty} = {literal}; let mut i = 0; while i < 3 {{ read(&x); await sleep(0); i = i + 1; }}"
            ),
            format!(
                "let x: {ty} = {literal}; read(&x); await sleep(0); let y: {ty} = {literal}; await sleep(0); println(y);"
            ),
            format!("let x: {ty} = {literal}; defer {{ read(&x); }} await sleep(0);"),
        ] {
            let source = format!(
                "fn read(value: &{ty}) -> {ty} {{ return value; }} async fn main() {{ {body} }}"
            );
            let f = main_function(&source);
            assert_stable(&f, "x");
        }
    }
}

#[test]
fn stable_borrow_parameters_shadowing_and_mutation() {
    let f = main_function(
        "fn read(value: &i64) -> i64 { return value; } async fn f(x: i64) { read(&x); await sleep(0); }",
    );
    assert_stable(&f, "x");
    let f = main_function(
        "fn inc(value: &mut i64) { value = value + 1; } async fn main() { let mut x = 1; inc(&x); await sleep(0); inc(&x); println(x); }",
    );
    assert_stable(&f, "x");
    let f = main_function(
        "fn read(value: &i64) -> i64 { return value; } async fn main() { let x = 1; read(&x); if true { let x = 2; read(&x); await sleep(0); } read(&x); }",
    );
    let borrowed: Vec<_> = f
        .locals
        .iter()
        .filter(|l| l.name == "x" || l.name.starts_with("x$"))
        .collect();
    assert!(borrowed.len() >= 2, "{:?}", f.locals);
    for local in borrowed {
        assert_stable(&f, &local.name);
    }
}

#[test]
fn stable_borrow_async_callee_planning_and_artifact() {
    // B2 exercises lower IR directly. E1707 remains the public frontend gate
    // until the async-reference parameter ABI lands in B3.
    let p = program(
        "async fn read(value: &i64) -> i64 { await sleep(0); return value; } async fn main() { let x = 42; let task = read(&x); await sleep(0); println(await task); }",
    );
    let f = p.functions.last().unwrap();
    assert_stable(f, "x");
    let encoded = serde_json::to_vec(&p).unwrap();
    let restored: super::super::LirProgram = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(restored, p);
    let dump = super::super::format_program(&p);
    assert!(dump.contains("frame slot"));
    assert!(dump.contains("`x`"));
}

#[test]
fn stable_borrow_rejects_missing_aliased_and_invalid_slots() {
    let original = main_function(
        "fn read(value: &i64) -> i64 { return value; } async fn main() { let x = 1; let y = 2; read(&x); await sleep(0); println(y); }",
    );
    let x = original.locals.iter().find(|l| l.name == "x").unwrap().id;
    let y = original.locals.iter().find(|l| l.name == "y").unwrap().id;
    for fault in 0..5 {
        let mut f = original.clone();
        let slot = f.async_frame.locals[&x];
        match fault {
            0 => {
                f.async_frame.locals.remove(&x);
            }
            1 => {
                f.async_frame.locals.insert(y, slot);
            }
            2 => {
                f.async_frame
                    .locals
                    .insert(x, FrameSlot { index: usize::MAX });
            }
            3 => {
                f.async_frame.slots[slot.index] = y;
            }
            4 => {
                for block in &mut f.blocks {
                    for inst in &mut block.instrs {
                        if let LirInst::Compute {
                            value: LirRvalue::DirectCall { args, .. },
                            ..
                        } = inst
                        {
                            for arg in args {
                                if let LirOperand::Reference { place, .. } = arg {
                                    *place = LirPlace::Local(LirLocalId(u32::MAX));
                                }
                            }
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
        assert!(stable_borrow_rejection(&f).is_some(), "fault {fault}");
    }
}

#[test]
fn stable_borrow_scaling_counts() {
    for count in [8, 32, 128] {
        for repeated in [false, true] {
            let mut source =
                String::from("fn read(value: &i64) -> i64 { return value; } async fn main() {");
            if repeated {
                source.push_str("let x = 42;");
            }
            for i in 0..count {
                if repeated {
                    source.push_str("read(&x);");
                } else {
                    source.push_str(&format!("let x{i} = {i}; read(&x{i});"));
                }
            }
            source.push_str("await sleep(0); }");
            BORROW_OPERAND_VISITS.with(|visits| visits.set(0));
            let f = main_function(&source);
            let visits = BORROW_OPERAND_VISITS.with(|visits| visits.get());
            let operands: usize = f
                .blocks
                .iter()
                .flat_map(|b| &b.instrs)
                .map(|inst| {
                    if let LirInst::Compute { value, .. } = inst {
                        value.operands().len()
                    } else {
                        0
                    }
                })
                .sum();
            assert_eq!(visits, operands, "one promotion visit per operand");
            let promoted = f.locals.iter().filter(|l| l.name.starts_with('x')).count();
            assert_eq!(promoted, if repeated { 1 } else { count });
            for local in f.locals.iter().filter(|l| l.name.starts_with('x')) {
                assert_stable(&f, &local.name);
            }
            let refs = f
                .blocks
                .iter()
                .flat_map(|b| &b.instrs)
                .filter_map(|i| {
                    if let LirInst::Compute { value, .. } = i {
                        Some(value.operands())
                    } else {
                        None
                    }
                })
                .flatten()
                .filter(|arg| matches!(arg, LirOperand::Reference { .. }))
                .count();
            assert_eq!(refs, count);
            println!(
                "borrow count={count} repeated={repeated} reference_operands={refs} promoted={promoted} operand_visits={visits}"
            );
        }
    }
}

#[test]
fn stable_borrow_cleanup_capture_requires_storage() {
    let mut f = main_function(
        "fn read(value: &i64) -> i64 { return value; } async fn main() { let x = 1; defer { read(&x); } await sleep(0); }",
    );
    assert_stable(&f, "x");
    let x = f.locals.iter().find(|l| l.name == "x").unwrap().id;
    f.async_frame.locals.remove(&x);
    assert!(
        stable_borrow_rejection(&f)
            .unwrap()
            .contains("exclusive stable")
    );
}
