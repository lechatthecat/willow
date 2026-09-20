use super::*;

fn register(codegen: &mut Codegen, source: &str) {
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    for item in program.items {
        if let Item::Class(class) = item {
            codegen.register_class_layout(&class).unwrap();
        }
    }
}

fn finalize(codegen: &mut Codegen) {
    codegen.finalize_class_layouts();
    codegen.finalize_class_vslots();
}

#[test]
fn reparent_updates_canonical_edges_fields_slots_and_descendants() {
    let mut cg = Codegen::new(&CompilerOptions::debug()).unwrap();
    register(
        &mut cg,
        "
        open class A { pub a: i64; pub open fn a(self) {} }
        open class X { pub x: bool; pub open fn x(self) {} }
        open class B extends A { pub b: i64; pub open fn b(self) {} }
        class C extends B { pub c: i64; }
        class U extends A { pub u: i64; }
    ",
    );
    finalize(&mut cg);
    cg.bind_canonical_type_alias("Parent", "X");
    cg.bind_canonical_type_alias("A", "X");
    cg.bind_canonical_type_alias("Child", "C");
    register(
        &mut cg,
        "open class B extends Parent { pub b: i64; pub open fn b(self) {} }",
    );
    assert_eq!(cg.dirty_class_layouts.len(), 2);
    assert_eq!(cg.dirty_class_vslots.len(), 2);
    let a = TypeId::from_source_name("A");
    let b = TypeId::from_source_name("B");
    let x = TypeId::from_source_name("X");
    assert!(!cg.class_dependents[&a].contains(&b));
    assert!(cg.class_dependents[&x].contains(&b));
    finalize(&mut cg);
    assert_eq!(
        cg.class_layouts.get("Child").unwrap(),
        &vec![
            ("x".into(), Type::Bool),
            ("b".into(), Type::I64),
            ("c".into(), Type::I64),
        ]
    );
    assert_eq!(cg.class_vslots.get("Child").unwrap().as_slice(), ["x", "b"]);
    assert_eq!(
        cg.class_vslots.get_canonical("U").unwrap().as_slice(),
        ["a"]
    );
    assert_eq!(cg.class_layouts.get_canonical("U").unwrap()[0].0, "a");
    assert_eq!(cg.type_scope.resolve(&a), x);

    register(
        &mut cg,
        "open class B { pub b: i64; pub open fn b(self) {} }",
    );
    assert!(cg.class_base.get_canonical("B").is_none());
    assert!(!cg.class_dependents.contains_key(&x));
    finalize(&mut cg);
    assert_eq!(
        cg.class_layouts.get("Child").unwrap(),
        &vec![("b".into(), Type::I64), ("c".into(), Type::I64),]
    );
    assert_eq!(cg.class_vslots.get("Child").unwrap().as_slice(), ["b"]);
    cg.invalidate_class_layout("A");
    assert_eq!(
        cg.dirty_class_layouts,
        HashSet::from([a, TypeId::from_source_name("U")])
    );
}

#[test]
fn reparent_edge_storage_and_invalidation_ignore_historical_parents() {
    for revisions in [1, 16, 64, 256] {
        let mut cg = Codegen::new(&CompilerOptions::debug()).unwrap();
        register(&mut cg, "open class B {} class C extends B {}");
        for i in 0..revisions {
            register(&mut cg, &format!("open class P{i} {{}}"));
        }
        finalize(&mut cg);
        cg.layout_work = [0; 5];
        for i in 0..revisions {
            register(&mut cg, &format!("open class B extends P{i} {{}}"));
            // Repeating the same declaration must not duplicate edges or walks.
            register(&mut cg, &format!("open class B extends P{i} {{}}"));
            assert_eq!(cg.class_dependents.len(), 2);
            assert_eq!(
                cg.class_dependents
                    .values()
                    .map(HashSet::len)
                    .sum::<usize>(),
                2
            );
            finalize(&mut cg);
        }
        assert_eq!(cg.layout_work[3], revisions * 2);
        assert_eq!(cg.layout_work[4], revisions);
        register(&mut cg, "open class B {}");
        finalize(&mut cg);
        assert_eq!(cg.class_dependents.len(), 1);
        assert_eq!(cg.class_base.len(), 1);
        cg.layout_work = [0; 5];
        for i in 0..revisions {
            cg.invalidate_class_layout(&format!("P{i}"));
        }
        assert_eq!(cg.layout_work[3], revisions);
        assert_eq!(cg.layout_work[4], 0);
        assert!(
            !cg.dirty_class_layouts
                .contains(&TypeId::from_source_name("B"))
        );
    }
}
