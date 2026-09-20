use super::*;

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
                let field = if shape == "redeclared" {
                    "field0".into()
                } else {
                    format!("field{i}")
                };
                source.push_str(&format!("open class C{i}{base} {{ pub {field}: i64; }}"));
            }
            let declarations = classes(&source);
            for reverse in [false, true] {
                let mut codegen = Codegen::new(&CompilerOptions::debug()).unwrap();
                for index in 0..size {
                    let i = if reverse { size - 1 - index } else { index };
                    codegen.register_class_layout(&declarations[i]).unwrap();
                }
                codegen.finalize_class_layouts();
                codegen.finalize_class_vslots();
                let copied = if shape == "chain" {
                    size * (size - 1) / 2
                } else {
                    size - 1
                };
                let edges = if reverse { size - 1 } else { 0 };
                assert_eq!(codegen.layout_work, [size, copied, size, size, edges]);
                for i in 0..size {
                    let fields = codegen
                        .class_layouts
                        .get_canonical(&format!("C{i}"))
                        .unwrap();
                    let expected: Vec<_> = match shape {
                        "chain" => (0..=i).map(|j| format!("field{j}")).collect(),
                        "fanout" if i > 0 => vec!["field0".into(), format!("field{i}")],
                        _ => vec!["field0".into()],
                    };
                    assert_eq!(
                        fields.iter().map(|(name, _)| name).collect::<Vec<_>>(),
                        expected.iter().collect::<Vec<_>>()
                    );
                }
                println!(
                    "shape={shape} size={size} reverse={reverse} work={:?}",
                    codegen.layout_work
                );
                codegen.layout_work = [0; 5];
                for _ in 0..8 {
                    codegen.finalize_class_layouts();
                }
                assert_eq!(codegen.layout_work, [0; 5]);
                // Repeated equivalent invalidations share descendant traversal.
                for _ in 0..8 {
                    codegen.invalidate_class_layout("C0");
                }
                assert_eq!(codegen.layout_work, [0, 0, 0, size, size - 1]);
                codegen.finalize_class_layouts();
                codegen.finalize_class_vslots();
                codegen.layout_work = [0; 5];
                codegen.invalidate_class_layout(&format!("C{}", size - 1));
                codegen.finalize_class_layouts();
                let inherited = if shape == "chain" {
                    size - 1
                } else {
                    usize::from(size > 1)
                };
                assert_eq!(codegen.layout_work, [1, inherited, 1, 1, 0]);
            }
        }
    }
}

#[test]
fn field_layout_updates_preserve_types_order_and_independent_dirty_passes() {
    let declarations = classes(
        "open class A { pub a: i64; } open class B extends A { pub a: bool; pub b: bool; } class C extends B { pub c: i64; }",
    );
    let mut codegen = Codegen::new(&CompilerOptions::debug()).unwrap();
    for class in declarations.iter().rev() {
        codegen.register_class_layout(class).unwrap();
    }
    codegen.finalize_class_layouts();
    // Slots remain dirty when fields are rebuilt after a parent update.
    let updated = classes("open class A { pub a: String; pub z: bool; }");
    codegen.register_class_layout(&updated[0]).unwrap();
    codegen.finalize_class_layouts();
    codegen.finalize_class_vslots();
    assert_eq!(
        codegen.class_layouts.get_canonical("C").unwrap(),
        &vec![
            ("a".into(), Type::String),
            ("z".into(), Type::Bool),
            ("b".into(), Type::Bool),
            ("c".into(), Type::I64),
        ]
    );
    // Conversely, field dirtiness cannot prevent slot invalidation.
    codegen.invalidate_class_layout("C");
    codegen.finalize_class_vslots();
    codegen.invalidate_class_layout("A");
    assert_eq!(codegen.dirty_class_vslots.len(), 3);
    codegen.finalize_class_layouts();
    codegen.finalize_class_vslots();
    // A newly registered child of an already-dirty parent still gets rebuilt.
    codegen.invalidate_class_layout("A");
    let child = classes("class D extends C { pub d: bool; }");
    codegen.register_class_layout(&child[0]).unwrap();
    codegen.finalize_class_layouts();
    assert_eq!(codegen.class_layouts.get_canonical("D").unwrap().len(), 5);
}

#[test]
fn field_layouts_keep_canonical_identity_under_aliases_and_cycle_fallback() {
    let mut codegen = Codegen::new(&CompilerOptions::debug()).unwrap();
    for class in classes(
        "open class A { pub a: i64; } class B extends A { pub b: bool; } class Other { pub other: String; }",
    ) {
        codegen.register_class_layout(&class).unwrap();
    }
    codegen.bind_type_alias("A", "Other");
    codegen.finalize_class_layouts();
    assert_eq!(
        codegen.class_layouts.get_canonical("B").unwrap(),
        &vec![("a".into(), Type::I64), ("b".into(), Type::Bool)]
    );
    let mut codegen = Codegen::new(&CompilerOptions::debug()).unwrap();
    for class in classes(
        "open class A extends B { pub a: i64; } open class B extends A { pub b: bool; } class C extends A { pub c: String; }",
    ) {
        codegen.register_class_layout(&class).unwrap();
    }
    codegen.finalize_class_layouts();
    for (name, expected) in [
        ("A", vec!["b", "a"]),
        ("B", vec!["a", "b"]),
        ("C", vec!["b", "a", "c"]),
    ] {
        assert_eq!(
            codegen
                .class_layouts
                .get_canonical(name)
                .unwrap()
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn field_layouts_do_not_inherit_unregistered_builtin_layouts() {
    let mut codegen = Codegen::new(&CompilerOptions::debug()).unwrap();
    for class in classes("class C extends PanicInfo { pub own: i64; }") {
        codegen.register_class_layout(&class).unwrap();
    }
    codegen.finalize_class_layouts();
    assert_eq!(
        codegen.class_layouts.get_canonical("C").unwrap(),
        &vec![("own".into(), Type::I64)]
    );
}
