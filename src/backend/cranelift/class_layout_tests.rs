use super::*;
use crate::semantic::method_slots::MethodSlots;

fn classes(source: &str) -> Vec<ClassDecl> {
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    program
        .items
        .into_iter()
        .filter_map(|item| match item {
            Item::Class(class) => Some(class),
            _ => None,
        })
        .collect()
}

/// The frozen layout under `name`'s DECLARATION identity, aliases ignored.
fn fields(codegen: &Codegen, name: &str) -> Option<std::sync::Arc<Vec<(String, Type)>>> {
    codegen
        .layout_queries
        .fields(TypeId::from_source_name(name))
}

fn slots(codegen: &Codegen, name: &str) -> std::sync::Arc<MethodSlots> {
    codegen
        .layout_queries
        .slots(TypeId::from_source_name(name))
        .unwrap()
}

fn field_names(codegen: &Codegen, name: &str) -> Vec<String> {
    fields(codegen, name)
        .unwrap()
        .iter()
        .map(|(name, _)| name.clone())
        .collect()
}

#[test]
fn field_layout_counts_are_output_sensitive_in_both_declaration_orders() {
    for size in [1, 16, 64, 256, 1024] {
        for shape in ["chain", "fanout", "redeclared"] {
            if shape == "chain" && size > 256 {
                continue;
            }
            let mut source = String::new();
            for i in 0..size {
                let base = if i == 0 {
                    String::new()
                } else {
                    format!(" extends C{}", if shape == "fanout" { 0 } else { i - 1 })
                };
                let (field, method) = if shape == "redeclared" {
                    ("field0".to_string(), "m0".to_string())
                } else {
                    (format!("field{i}"), format!("m{i}"))
                };
                let modifier = if i == 0 || shape == "fanout" {
                    "open"
                } else {
                    "override"
                };
                source.push_str(&format!(
                    "open class C{i}{base} {{ pub {field}: i64; pub {modifier} fn {method}(self) {{}} }}"
                ));
            }
            let declarations = classes(&source);
            for reverse in [false, true] {
                let mut codegen = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
                for index in 0..size {
                    let i = if reverse { size - 1 - index } else { index };
                    codegen.register_class_layout(&declarations[i]).unwrap();
                }
                codegen.finalize_class_layouts().unwrap();
                let copied = if shape == "chain" {
                    size * (size - 1) / 2
                } else {
                    size - 1
                };
                // One visit per declaration, one copy per inherited entry, one
                // insertion per own declaration: independent of registration order.
                assert_eq!(codegen.layout_queries.declaration_visits(), size);
                assert_eq!(codegen.layout_queries.work(), [copied, size, copied, size]);
                for i in 0..size {
                    let expected: Vec<String> = match shape {
                        "chain" => (0..=i).map(|j| format!("field{j}")).collect(),
                        "fanout" if i > 0 => vec!["field0".into(), format!("field{i}")],
                        _ => vec!["field0".into()],
                    };
                    assert_eq!(field_names(&codegen, &format!("C{i}")), expected);
                    let slots = slots(&codegen, &format!("C{i}"));
                    let expected_slots = match shape {
                        "chain" => (0..=i).map(|j| format!("m{j}")).collect(),
                        "fanout" if i > 0 => vec!["m0".into(), format!("m{i}")],
                        _ => vec!["m0".into()],
                    };
                    assert_eq!(slots.as_slice(), expected_slots.as_slice());
                }
                println!(
                    "shape={shape} size={size} reverse={reverse} visits={} work={:?}",
                    codegen.layout_queries.declaration_visits(),
                    codegen.layout_queries.work()
                );
                // Nothing pending: repeated finalization does no work.
                for _ in 0..8 {
                    codegen.finalize_class_layouts().unwrap();
                }
                assert_eq!(codegen.layout_queries.declaration_visits(), size);
                assert_eq!(codegen.layout_queries.work(), [copied, size, copied, size]);
                // Re-registering an already frozen declaration requests the same
                // completed result without revisiting its ancestors.
                for _ in 0..8 {
                    codegen
                        .register_class_layout(&declarations[size - 1])
                        .unwrap();
                }
                codegen.finalize_class_layouts().unwrap();
                assert_eq!(codegen.layout_queries.declaration_visits(), size);
                assert_eq!(codegen.layout_queries.work(), [copied, size, copied, size]);
            }
        }
    }
}

#[test]
fn field_layouts_preserve_inherited_types_and_order_under_redeclaration() {
    let declarations = classes(
        "open class A { pub a: i64; pub open fn a(self) {} } open class B extends A { pub a: bool; pub b: bool; pub override fn a(self) {} pub open fn b(self) {} } class C extends B { pub c: i64; pub override fn b(self) {} }",
    );
    let mut codegen = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
    for class in declarations.iter().rev() {
        codegen.register_class_layout(class).unwrap();
    }
    codegen.finalize_class_layouts().unwrap();
    assert_eq!(
        fields(&codegen, "C").unwrap().as_ref(),
        &vec![
            ("a".into(), Type::I64),
            ("b".into(), Type::Bool),
            ("c".into(), Type::I64),
        ]
    );
    let slots = slots(&codegen, "C");
    assert_eq!(slots.as_slice(), ["a", "b"]);
    assert_eq!(slots.slot_of("a"), Some(0));
    assert_eq!(slots.slot_of("b"), Some(1));
    // A later unit extends the frozen chain without reopening it.
    let visits = codegen.layout_queries.declaration_visits();
    let child = classes("class D extends C { pub d: bool; }");
    codegen.register_class_layout(&child[0]).unwrap();
    codegen.finalize_class_layouts().unwrap();
    assert_eq!(field_names(&codegen, "D"), ["a", "b", "c", "d"]);
    assert_eq!(codegen.layout_queries.declaration_visits(), visits + 1);
}

#[test]
fn field_layouts_keep_canonical_identity_under_aliases_and_reject_cycles() {
    let mut codegen = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
    for class in classes(
        "open class A { pub a: i64; } class B extends A { pub b: bool; } class Other { pub other: String; }",
    ) {
        codegen.register_class_layout(&class).unwrap();
    }
    // The base edge was resolved at registration; a later alias cannot move it.
    codegen.bind_type_alias("A", "Other");
    codegen.finalize_class_layouts().unwrap();
    assert_eq!(
        fields(&codegen, "B").unwrap().as_ref(),
        &vec![("a".into(), Type::I64), ("b".into(), Type::Bool)]
    );
    // A cyclic `extends` is a checker error; a backend driven directly with
    // one gets a diagnosed failure instead of a partial layout.
    let mut codegen = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
    for class in classes(
        "open class A extends B { pub a: i64; } open class B extends A { pub b: bool; } class C extends A { pub c: String; }",
    ) {
        codegen.register_class_layout(&class).unwrap();
    }
    for _ in 0..2 {
        let error = codegen.finalize_class_layouts().unwrap_err().to_string();
        assert!(error.contains("cyclic class layout"), "{error}");
    }
    assert!(fields(&codegen, "A").is_none());
    assert!(fields(&codegen, "C").is_none());
}

#[test]
fn field_layouts_do_not_inherit_unregistered_builtin_layouts() {
    let mut codegen = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
    for class in classes("class C extends PanicInfo { pub own: i64; }") {
        codegen.register_class_layout(&class).unwrap();
    }
    codegen.finalize_class_layouts().unwrap();
    assert_eq!(
        fields(&codegen, "C").unwrap().as_ref(),
        &vec![("own".into(), Type::I64)]
    );
    // The builtin keeps its own layout for panic handlers, without a runtime
    // type id: it is never allocated by user code.
    assert_eq!(
        field_names(&codegen, "PanicInfo"),
        ["message", "file", "line", "column"]
    );
    assert!(codegen.classes().type_id("PanicInfo").is_none());
}
