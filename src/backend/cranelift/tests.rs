#[test]
fn cancellation_defer_snapshots_do_not_alias_divergent_mutations() {
    let mut original = DeferStack::default();
    original.push(Vec::new());
    let mut left = original.clone();
    let mut right = original.clone();
    assert_eq!(left.identity, right.identity);
    left.push(Vec::new());
    right.push(Vec::new());
    assert_ne!(left.identity, right.identity);
    assert_ne!(left.identity, original.identity);
    assert_ne!(right.identity, original.identity);
    let snapshot = right.clone();
    right = original.clone();
    assert_eq!(right.identity, original.identity);
    left = DeferStack::default();
    assert_ne!(right.identity, snapshot.identity);
    assert_ne!(left.identity, right.identity);
    assert_eq!(original.len(), 1);
    assert_eq!(snapshot.len(), 2);
}

#[test]
fn class_layouts_complete_subclass_first_declarations_from_frozen_queries() {
    let source = "
            class Leaf extends Middle { pub c: bool; pub override fn one(self) {} }
            open class Root { pub a: i64; pub open fn one(self) {} }
            open class Middle extends Root { pub b: f64; pub open fn two(self) {} }
        ";
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let mut session_build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
    let mut session = session_build.declaration_context();
    for item in &program.items {
        let Item::Class(class) = item else {
            panic!("expected class")
        };
        session.register_class_layout(class).unwrap();
        // Registration never installs a provisional layout: nothing may
        // read an inherited field before the chain is complete.
        let id = TypeId::from_source_name(&class.name);
        assert!(!session.layout_queries.has_layout(id));
        assert!(session.layout_queries.slots(id).is_none());
    }
    session.finalize_class_layouts().unwrap();
    fn fields(session: &UnitCodegenContext<'_>, name: &str) -> std::sync::Arc<Vec<(String, Type)>> {
        session
            .layout_queries
            .fields(TypeId::from_source_name(name))
            .unwrap()
    }
    fn names(session: &UnitCodegenContext<'_>, name: &str) -> Vec<String> {
        fields(session, name)
            .iter()
            .map(|(field, _)| field.clone())
            .collect()
    }
    fn slots(
        session: &UnitCodegenContext<'_>,
        name: &str,
    ) -> std::sync::Arc<crate::semantic::method_slots::MethodSlots> {
        session
            .layout_queries
            .slots(TypeId::from_source_name(name))
            .unwrap()
    }
    fn runtime_id(session: &UnitCodegenContext<'_>, name: &str) -> Option<i64> {
        session
            .layout_queries
            .runtime_id(TypeId::from_source_name(name))
    }
    assert_eq!(names(&session, "Root"), ["a"]);
    assert_eq!(names(&session, "Middle"), ["a", "b"]);
    assert_eq!(names(&session, "Leaf"), ["a", "b", "c"]);
    assert_eq!(fields(&session, "Leaf")[1].1, Type::F64);
    assert_eq!(slots(&session, "Root").as_slice(), ["one"]);
    assert_eq!(slots(&session, "Middle").as_slice(), ["one", "two"]);
    assert_eq!(slots(&session, "Leaf").as_slice(), ["one", "two"]);
    assert_eq!(slots(&session, "Leaf").slot_of("one"), Some(0));
    // Runtime ids follow registration order, not the finalized chain.
    assert_eq!(runtime_id(&session, "Leaf"), Some(1));
    assert_eq!(runtime_id(&session, "Root"), Some(2));
    assert_eq!(runtime_id(&session, "Middle"), Some(3));
    assert_eq!(session.layout_queries.declaration_visits(), 3);
    let work = session.layout_queries.work();
    // Re-registering a frozen declaration under a different base or
    // field list is a request for the same result, not a reparent.
    let updated = crate::parser::Parser::new(
        crate::lexer::Lexer::new("class Leaf extends Root { pub z: bool; }")
            .tokenize()
            .unwrap(),
    )
    .parse()
    .0;
    let Item::Class(updated) = &updated.items[0] else {
        panic!("expected class")
    };
    for _ in 0..8 {
        session.register_class_layout(updated).unwrap();
        session.finalize_class_layouts().unwrap();
    }
    assert_eq!(names(&session, "Leaf"), ["a", "b", "c"]);
    assert_eq!(runtime_id(&session, "Leaf"), Some(1));
    assert_eq!(session.layout_queries.declaration_visits(), 3);
    assert_eq!(session.layout_queries.work(), work);
}

#[test]
fn class_base_queries_keep_canonical_edges_across_scope_changes() {
    let tokens = crate::lexer::Lexer::new("class Root {} class Child extends Alias {}")
        .tokenize()
        .unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let Item::Class(root) = &program.items[0] else {
        panic!("expected class")
    };
    let Item::Class(child) = &program.items[1] else {
        panic!("expected class")
    };
    let queries = std::rc::Rc::new(crate::compiler_db::layout::LayoutQueries::default());
    let mut codegen_build =
        Codegen::new(&CompilerOptions::debug(), std::rc::Rc::clone(&queries)).unwrap();
    let mut codegen = codegen_build.declaration_context();
    codegen.bind_canonical_type_alias("Alias", "Root");
    // The parent need not be registered for its identity to be final.
    codegen.register_class_layout(child).unwrap();
    codegen.register_class_layout(root).unwrap();
    codegen.bind_canonical_type_alias("Alias", "Other");
    let mut canonical_child = child.clone();
    canonical_child.base_class = Some(TypePath::Local("Root".into()));
    codegen.register_class_layout(&canonical_child).unwrap();
    let root_id = TypeId::local("Root");
    let child_id = TypeId::local("Child");
    assert_eq!(codegen.layout_queries.base(child_id), Some(root_id));
    assert_eq!(
        queries
            .class_base(child_id, || panic!("edge recomputed"))
            .unwrap(),
        Some(root_id)
    );
    assert_eq!(
        queries
            .class_base(root_id, || panic!("root recomputed"))
            .unwrap(),
        None
    );
}

#[test]
fn generated_class_type_ids_use_shared_query_identity_across_backends() {
    let tokens = crate::lexer::Lexer::new("class A {} class B {}")
        .tokenize()
        .unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let queries = std::rc::Rc::new(crate::compiler_db::layout::LayoutQueries::default());
    let mut first_build =
        Codegen::new(&CompilerOptions::debug(), std::rc::Rc::clone(&queries)).unwrap();
    let mut first = first_build.declaration_context();
    for item in &program.items {
        let Item::Class(class) = item else {
            panic!("expected class")
        };
        first.register_class_layout(class).unwrap();
    }
    let mut second_build = Codegen::new(&CompilerOptions::debug(), queries).unwrap();
    let mut second = second_build.declaration_context();
    for item in program.items.iter().rev() {
        let Item::Class(class) = item else {
            panic!("expected class")
        };
        second.register_class_layout(class).unwrap();
        let id = TypeId::from_source_name(&class.name);
        assert_eq!(
            second.layout_queries.runtime_id(id),
            first.layout_queries.runtime_id(id)
        );
    }
}

#[test]
fn generated_class_type_ids_preserve_identity_across_redeclaration_and_aliases() {
    for count in [1, 16, 64, 256] {
        let source = (0..count)
            .map(|i| format!("class C{i} {{}}"))
            .collect::<String>();
        let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut codegen_build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
        let mut codegen = codegen_build.declaration_context();
        for (i, item) in program.items.iter().enumerate() {
            let Item::Class(class) = item else {
                panic!("expected class")
            };
            codegen.register_class_layout(class).unwrap();
            codegen.register_class_layout(class).unwrap();
            let alias = format!("Alias{i}");
            codegen.bind_canonical_type_alias(&alias, &class.name);
            assert_eq!(codegen.classes().type_id(&class.name), Some(i as i64 + 1));
            assert_eq!(
                codegen.classes().type_id(alias.as_str()),
                Some(i as i64 + 1)
            );
            assert_eq!(codegen.layout_queries.runtime_classes().len(), i + 1);
        }
        assert_eq!(codegen.layout_queries.runtime_classes().len(), count);
    }
}

#[test]
fn production_stack_probes_use_inline_four_kib_pages() {
    let mut codegen_build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
    let codegen = codegen_build.declaration_context();
    let flags = codegen.output.module.isa().flags();
    assert!(flags.enable_probestack());
    assert_eq!(
        flags.probestack_strategy(),
        settings::ProbestackStrategy::Inline
    );
    assert_eq!(flags.probestack_size_log2(), 12);
}

#[test]
fn closure_type_canonicalization_perspectives() {
    use super::*;

    let mut codegen_build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
    let mut codegen = codegen_build.declaration_context();
    codegen.enum_infos.insert(
        "origin::Choice",
        EnumInfo {
            name: "origin::Choice".into(),
            public: true,
            type_params: vec![],
            declaration_span: Span::dummy(),
            variants: vec![],
        },
    );
    codegen.bind_canonical_type_alias("Alias", "origin::Choice");
    codegen.bind_type_alias("Forward", "Alias");
    codegen.layout_queries.register_class(
        TypeId::from_source_name("origin::Record"),
        None,
        vec![],
        vec![],
    );
    codegen.finalize_class_layouts().unwrap();
    codegen.bind_canonical_type_alias("Record", "origin::Record");

    let named = |name: &str| Type::Named(name.into());
    let closure = |params, ret| Type::Closure(params, Box::new(ret));
    let function = |params, ret| Type::Fn(params, Box::new(ret));
    let array = |ty| Type::Array(Box::new(ty));
    let generic = |ty| Type::Generic("Container".into(), vec![ty]);
    let alias = named("Alias");
    let canonical = named("origin::Choice");
    // Each row checks BOTH signature enum normalization and declared layout
    // normalization: 24 explicit perspectives, including unchanged Fn paths.
    let cases = [
        (
            "closure parameter",
            closure(vec![alias.clone()], Type::Void),
            closure(vec![canonical.clone()], Type::Void),
        ),
        (
            "closure result",
            closure(vec![], alias.clone()),
            closure(vec![], canonical.clone()),
        ),
        (
            "array parameter",
            closure(vec![array(alias.clone())], Type::Void),
            closure(vec![array(canonical.clone())], Type::Void),
        ),
        (
            "array result",
            closure(vec![], array(alias.clone())),
            closure(vec![], array(canonical.clone())),
        ),
        (
            "generic parameter",
            closure(vec![generic(alias.clone())], Type::Void),
            closure(vec![generic(canonical.clone())], Type::Void),
        ),
        (
            "generic result",
            closure(vec![], generic(alias.clone())),
            closure(vec![], generic(canonical.clone())),
        ),
        (
            "function parameter",
            closure(
                vec![function(vec![alias.clone()], alias.clone())],
                Type::Void,
            ),
            closure(
                vec![function(vec![canonical.clone()], canonical.clone())],
                Type::Void,
            ),
        ),
        (
            "function result",
            closure(vec![], function(vec![alias.clone()], alias.clone())),
            closure(vec![], function(vec![canonical.clone()], canonical.clone())),
        ),
        (
            "nested closure",
            closure(
                vec![closure(vec![alias.clone()], alias.clone())],
                closure(vec![], alias.clone()),
            ),
            closure(
                vec![closure(vec![canonical.clone()], canonical.clone())],
                closure(vec![], canonical.clone()),
            ),
        ),
        (
            "closure inside function",
            function(
                vec![closure(vec![alias.clone()], alias.clone())],
                closure(vec![], alias.clone()),
            ),
            function(
                vec![closure(vec![canonical.clone()], canonical.clone())],
                closure(vec![], canonical.clone()),
            ),
        ),
        (
            "forward alias",
            closure(vec![named("Forward")], named("Forward")),
            closure(vec![canonical.clone()], canonical.clone()),
        ),
        (
            "canonical unchanged",
            closure(vec![canonical.clone()], canonical.clone()),
            closure(vec![canonical.clone()], canonical),
        ),
    ];
    for (perspective, input, expected) in cases {
        assert_eq!(
            codegen.canonical_enum_type(&input),
            expected,
            "signature: {perspective}"
        );
        assert_eq!(
            codegen.canonical_declared_type(&input),
            expected,
            "layout: {perspective}"
        );
    }
    // Signature normalization intentionally preserves class spellings; the
    // layout flow resolves them, just as both flows already do for Fn.
    let record = closure(vec![named("Record")], named("Record"));
    assert_eq!(codegen.canonical_enum_type(&record), record);
    assert_eq!(
        codegen.canonical_declared_type(&record),
        closure(vec![named("origin::Record")], named("origin::Record"))
    );
    let unchanged = closure(
        vec![Type::I64, Type::Bool, Type::String, named("Unknown")],
        Type::Never,
    );
    assert_eq!(codegen.canonical_enum_type(&unchanged), unchanged);
    assert_eq!(codegen.canonical_declared_type(&unchanged), unchanged);
}
#[test]
fn later_units_complete_only_their_own_class_declarations() {
    for length in [1, 4, 10, 64] {
        let mut source = String::new();
        for i in 0..length {
            let base = if i == 0 {
                String::new()
            } else {
                format!(" extends C{}", i - 1)
            };
            source.push_str(&format!("open class C{i}{base} {{ pub f{i}: i64; pub open fn m{i}(self) -> i64 {{ return {i}; }} }}"));
        }
        let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let classes: Vec<_> = program
            .items
            .iter()
            .filter_map(|item| {
                if let Item::Class(class) = item {
                    Some(class)
                } else {
                    None
                }
            })
            .collect();
        // Dependencies are declared before dependents, one unit each: the
        // pass for unit i visits declaration i alone and copies its parent.
        let mut codegen_build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
        let mut codegen = codegen_build.declaration_context();
        for (i, class) in classes.iter().enumerate() {
            codegen.register_class_layout(class).unwrap();
            codegen.finalize_class_layouts().unwrap();
            assert_eq!(codegen.layout_queries.declaration_visits(), i + 1);
            assert_eq!(
                codegen.layout_queries.work(),
                [i * (i + 1) / 2, i + 1, i * (i + 1) / 2, i + 1]
            );
            codegen.finalize_class_layouts().unwrap();
            assert_eq!(codegen.layout_queries.declaration_visits(), i + 1);
        }
        // One unit declaring the whole chain subclass-first: one pass, one
        // visit per declaration, same layouts.
        let mut reversed_build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
        let mut reversed = reversed_build.declaration_context();
        for class in classes.iter().rev() {
            reversed.register_class_layout(class).unwrap();
        }
        reversed.finalize_class_layouts().unwrap();
        assert_eq!(reversed.layout_queries.declaration_visits(), length);
        assert_eq!(
            reversed.layout_queries.work(),
            codegen.layout_queries.work()
        );
        for i in 0..length {
            let name = format!("C{i}");
            for codegen in [&codegen, &reversed] {
                let id = TypeId::from_source_name(&name);
                let fields = codegen.layout_queries.fields(id).unwrap();
                let slots = codegen.layout_queries.slots(id).unwrap();
                assert_eq!(fields.len(), i + 1);
                assert_eq!(slots.len(), i + 1);
                for j in 0..=i {
                    assert_eq!(fields[j].0, format!("f{j}"));
                    assert_eq!(slots.slot_of(&format!("m{j}")), Some(j));
                }
            }
        }
    }
}

use super::*;
use crate::diagnostics::Span;

/// The backend refuses a non-64-bit target rather than emitting code whose
/// references are wider than the machine's pointers. The compiler always
/// targets the host, so the check that matters here is that the host it
/// accepted really is 64-bit — and that the function-address width the
/// emitter bakes in agrees with that host's pointer width.
#[test]
fn the_accepted_target_is_64_bit_and_agrees_with_the_function_address_width() {
    let isa_builder = cranelift_native::builder().expect("host ISA");
    let isa = isa_builder
        .finish(settings::Flags::new(settings::builder()))
        .expect("host ISA flags");
    assert_eq!(
        isa.pointer_bits(),
        64,
        "willow only supports 64-bit targets; `Codegen::new` rejects the rest"
    );
    assert_eq!(
        isa.pointer_type(),
        type_helpers::reference_type(isa.frontend_config()),
        "a function address must be exactly as wide as a pointer on an accepted target"
    );
}

#[test]
fn unit_async_codegen_02_sleep_builtin_lowers_to_runtime_sleep() {
    assert_eq!(
        builtin_call_runtime_name("sleep"),
        Some(rt::willow_timer_value_sleep)
    );
}

#[test]
fn unit_async_codegen_02c_yield_builtin_lowers_to_runtime_yield() {
    assert_eq!(
        builtin_call_runtime_name("yield"),
        Some(rt::willow_timer_value_yield)
    );
}

#[test]
fn unit_async_codegen_04_channel_element_type_extracts_generic_argument() {
    assert_eq!(
        channel_element_type(&Type::Generic(
            "Channel".to_string().into(),
            vec![Type::I64]
        )),
        Some(Type::I64)
    );
    assert_eq!(channel_element_type(&Type::I64), None);
}

#[test]
fn unit_async_codegen_05_channel_runtime_symbol_selects_primitive_or_pointer_abi() {
    assert_eq!(
        channel_runtime_symbol(&Type::I64, ["i64", "bool", "f64", "ptr"]),
        "i64"
    );
    assert_eq!(
        channel_runtime_symbol(&Type::Bool, ["i64", "bool", "f64", "ptr"]),
        "bool"
    );
    assert_eq!(
        channel_runtime_symbol(&Type::F64, ["i64", "bool", "f64", "ptr"]),
        "f64"
    );
    assert_eq!(
        channel_runtime_symbol(&Type::String, ["i64", "bool", "f64", "ptr"]),
        "ptr"
    );
    assert_eq!(
        channel_runtime_symbol(
            &Type::Named("Node".to_string().into()),
            ["i64", "bool", "f64", "ptr"]
        ),
        "ptr"
    );
}

#[test]
fn unit_async_codegen_07_future_uses_runtime_pointer_abi() {
    assert_eq!(
        clif_type(
            types::I64,
            &Type::Generic("Future".to_string().into(), vec![Type::I64])
        ),
        types::I64
    );
    assert_eq!(
        clif_type(
            types::I64,
            &Type::Generic("Future".to_string().into(), vec![Type::Void])
        ),
        types::I64
    );
}

#[test]
fn class_gc_ref_mask_allows_first_63_fields() {
    let layout: Vec<(String, Type)> = (0..OBJECT_FIELD_MASK_CAPACITY)
        .map(|i| (format!("f{i}"), Type::String))
        .collect();
    let mask = gc_ref_mask_for_layout(&layout, &TypeMap::new());
    // Word 0 is the class descriptor, so fields occupy mask bits 1..63.
    assert_eq!(mask, u64::MAX << 1);
}

#[test]
fn class_gc_ref_mask_uses_bitmap_beyond_coverage() {
    let mut fields: Vec<(String, Type)> = (0..64).map(|i| (format!("n{i}"), Type::I64)).collect();
    fields.push(("late".into(), Type::String));
    let object = crate::compiler_db::layout::ObjectLayout::new(fields.into(), 8);
    let layout = gc_codegen::GcLayoutMetadata::class(1, &object, &TypeMap::new());
    assert_eq!(layout.gc_ref_mask, 0);
    assert_eq!(layout.bitmap, [0, 2]);
}

// ── Async frame GC metadata ──────────────────────────────
//
// Each test is one perspective on the GC reference mask the compiler must
// hand to willow_async_frame_alloc: which frame slots are heap references.

#[test]
fn opaque_gc_owner_frame_slots_are_traced_without_language_types() {
    use crate::ir::lowered::LirStorageKind;
    let layout = AsyncFrameLayout::try_new(
        vec![
            AsyncFrameSlot {
                storage_kind: LirStorageKind::GcOwner,
                source_span: None,
                name: "owner".into(),
                ty: Type::Void,
            },
            AsyncFrameSlot {
                storage_kind: LirStorageKind::Value,
                source_span: None,
                name: "unit".into(),
                ty: Type::Void,
            },
        ],
        &TypeMap::new(),
    )
    .unwrap();
    assert!(layout.slot_is_gc_ref(0));
    assert!(!layout.slot_is_gc_ref(1));
    assert_eq!(layout.gc_slot_mask, 1);
}

/// Helper: build a layout from `(name, ty)` slots with no enum registry.
fn frame_layout(slots: &[(&str, Type)]) -> AsyncFrameLayout {
    let enum_infos: TypeMap<EnumInfo> = TypeMap::new();
    frame_layout_with(slots, &enum_infos)
}

fn frame_layout_with(slots: &[(&str, Type)], enum_infos: &TypeMap<EnumInfo>) -> AsyncFrameLayout {
    let slots = slots
        .iter()
        .map(|(n, t)| AsyncFrameSlot {
            storage_kind: crate::ir::lowered::LirStorageKind::Value,
            source_span: None,
            name: (*n).to_string(),
            ty: t.clone(),
        })
        .collect();
    AsyncFrameLayout::try_new(slots, enum_infos).unwrap()
}

/// Helper: an EnumInfo registry with one enum of the given (name, payload) variants.
fn enum_infos_with(name: &str, variants: &[(&str, Vec<Type>)]) -> TypeMap<'static, EnumInfo> {
    let mut map = TypeMap::new();
    map.insert(
        name.to_string(),
        EnumInfo {
            name: name.to_string().into(),
            public: true,
            type_params: vec![],
            declaration_span: Span::dummy(),
            variants: variants
                .iter()
                .enumerate()
                .map(|(i, (vn, pts))| crate::semantic::symbols::EnumVariantInfo {
                    name: (*vn).to_string(),
                    payload_types: pts.clone(),
                    tag: i as i64,
                    declaration_span: Span::dummy(),
                })
                .collect(),
        },
    );
    map
}

// 1. Empty frame → no slots, empty mask.
#[test]
fn async_frame_01_empty_layout_has_zero_mask() {
    let layout = frame_layout(&[]);
    assert_eq!(layout.slot_count(), 0);
    assert_eq!(layout.gc_slot_mask, 0);
}

// 2–4. Scalar slots are never GC references.
#[test]
fn async_frame_02_i64_slot_not_traced() {
    assert_eq!(frame_layout(&[("a", Type::I64)]).gc_slot_mask, 0);
}

#[test]
fn async_frame_03_bool_slot_not_traced() {
    assert_eq!(frame_layout(&[("a", Type::Bool)]).gc_slot_mask, 0);
}

#[test]
fn async_frame_04_f64_slot_not_traced() {
    assert_eq!(frame_layout(&[("a", Type::F64)]).gc_slot_mask, 0);
}

// 5. void slot is not traced.
#[test]
fn async_frame_05_void_slot_not_traced() {
    assert_eq!(frame_layout(&[("a", Type::Void)]).gc_slot_mask, 0);
}

// 6. A class reference (named, non-enum) is traced.
#[test]
fn async_frame_06_class_slot_traced() {
    let layout = frame_layout(&[("node", Type::Named("Node".to_string().into()))]);
    assert_eq!(layout.gc_slot_mask, 0b1);
    assert!(layout.slot_is_gc_ref(0));
}

// 7. A string slot is traced (GC-managed WillowString).
#[test]
fn async_frame_07_string_slot_traced() {
    assert_eq!(frame_layout(&[("s", Type::String)]).gc_slot_mask, 0b1);
}

// 8–9. Arrays of any element type are traced (handle + buffer are heap objects).
#[test]
fn async_frame_08_array_of_scalar_slot_traced() {
    let ty = Type::Array(Box::new(Type::I64));
    assert_eq!(frame_layout(&[("xs", ty)]).gc_slot_mask, 0b1);
}

#[test]
fn async_frame_09_array_of_ref_slot_traced() {
    let ty = Type::Array(Box::new(Type::String));
    assert_eq!(frame_layout(&[("xs", ty)]).gc_slot_mask, 0b1);
}

// 10. `T?` is canonical Option<T>; a reference payload may use the
// nullable-pointer niche and is traced (the runtime skips zero).
#[test]
fn async_frame_10_optional_ref_slot_traced() {
    let ty = Type::Generic(
        "Option".to_string().into(),
        vec![Type::Named("Node".to_string().into())],
    );
    assert_eq!(frame_layout(&[("maybe", ty)]).gc_slot_mask, 0b1);
}

// 11. Option<i64> needs a tagged enum allocation and is therefore traced.
#[test]
fn async_frame_11_optional_primitive_slot_untraced() {
    let ty = Type::Generic("Option".to_string().into(), vec![Type::I64]);
    assert_eq!(frame_layout(&[("maybe", ty)]).gc_slot_mask, 0);
}

// 12. Nested Option preserves both absence levels in a boxed outer value.
#[test]
fn async_frame_12_nested_optional_ref_traced() {
    let ty = Type::Generic(
        "Option".to_string().into(),
        vec![Type::Generic(
            "Option".to_string().into(),
            vec![Type::String],
        )],
    );
    assert_eq!(frame_layout(&[("m", ty)]).gc_slot_mask, 0b1);
}

// 13. Future is an opaque runtime pointer (no GcHeader) and is NOT traced
//     from a frame slot; Channel became a GC OBJECT and
//     Task/JoinHandle are GC async frames — all three ARE traced.
#[test]
fn async_frame_13_runtime_pointer_generics_and_joinhandle() {
    let future = Type::Generic("Future".to_string().into(), vec![Type::I64]);
    let channel = Type::Generic("Channel".to_string().into(), vec![Type::String]);
    let task = Type::Generic("Task".to_string().into(), vec![Type::I64]);
    let join = Type::Generic("JoinHandle".to_string().into(), vec![Type::Void]);
    assert_eq!(frame_layout(&[("f", future)]).gc_slot_mask, 0);
    assert_eq!(frame_layout(&[("c", channel)]).gc_slot_mask, 0b1);
    assert_eq!(frame_layout(&[("t", task)]).gc_slot_mask, 0b1);
    assert_eq!(frame_layout(&[("j", join)]).gc_slot_mask, 0b1);
}

// 14. Option<i64> (a generic enum carrying payload) is a heap object → traced.
#[test]
fn async_frame_14_option_generic_enum_untraced() {
    let ty = Type::Generic("Option".to_string().into(), vec![Type::I64]);
    assert_eq!(frame_layout(&[("o", ty)]).gc_slot_mask, 0);
}

// 15. Result<String,i64> is a heap object → traced.
#[test]
fn async_frame_15_result_generic_enum_traced() {
    let ty = Type::Generic("Result".to_string().into(), vec![Type::String, Type::I64]);
    assert_eq!(frame_layout(&[("r", ty)]).gc_slot_mask, 0b1);
}

// 16. A fieldless enum lowers to an immediate tag → NOT traced.
#[test]
fn async_frame_16_fieldless_enum_not_traced() {
    let enums = enum_infos_with(
        "Color",
        &[("Red", vec![]), ("Green", vec![]), ("Blue", vec![])],
    );
    let layout = frame_layout_with(&[("c", Type::Named("Color".to_string().into()))], &enums);
    assert_eq!(layout.gc_slot_mask, 0);
}

// 17. A with-payload enum is heap-allocated → traced.
#[test]
fn async_frame_17_payload_enum_traced() {
    let enums = enum_infos_with("Shape", &[("Dot", vec![]), ("Circle", vec![Type::I64])]);
    let layout = frame_layout_with(&[("s", Type::Named("Shape".to_string().into()))], &enums);
    assert_eq!(layout.gc_slot_mask, 0b1);
}

// 18. Mixed slots: only the GC-reference slots set their bit, by slot index.
#[test]
fn async_frame_18_mixed_slots_mask_by_index() {
    let layout = frame_layout(&[
        ("count", Type::I64),                             // slot 0 — not traced
        ("node", Type::Named("Node".to_string().into())), // slot 1 — traced
        ("ok", Type::Bool),                               // slot 2 — not traced
        ("name", Type::String),                           // slot 3 — traced
    ]);
    assert_eq!(layout.gc_slot_mask, 0b1010);
    assert!(!layout.slot_is_gc_ref(0));
    assert!(layout.slot_is_gc_ref(1));
    assert!(!layout.slot_is_gc_ref(2));
    assert!(layout.slot_is_gc_ref(3));
    assert_eq!(layout.slot_count(), 4);
}

// 19. The mask is slot-relative: a reference at slot K sets bit K (the runtime
//     allocator applies the 3-word header shift, not the compiler).
#[test]
fn async_frame_19_mask_is_slot_relative() {
    let layout = frame_layout(&[
        ("a", Type::I64),
        ("b", Type::I64),
        ("c", Type::I64),
        ("ref", Type::String), // slot 3
    ]);
    assert_eq!(layout.gc_slot_mask, 1u64 << 3);
}

// 20. GC slots beyond the inline mask use scalable metadata.
#[test]
fn async_frame_20_gc_slots_beyond_runtime_mask_are_traced() {
    let mut slots: Vec<(&str, Type)> = Vec::new();
    for _ in 0..ASYNC_FRAME_GC_SLOT_CAPACITY {
        slots.push(("r", Type::String));
    }
    let layout = frame_layout(&slots);
    assert_eq!(
        layout.gc_slot_mask,
        (1u64 << ASYNC_FRAME_GC_SLOT_CAPACITY) - 1
    );

    let too_many_slots: Vec<AsyncFrameSlot> = (0..=ASYNC_FRAME_GC_SLOT_CAPACITY)
        .map(|i| AsyncFrameSlot {
            storage_kind: crate::ir::lowered::LirStorageKind::Value,
            source_span: None,
            name: format!("r{i}"),
            ty: Type::String,
        })
        .collect();
    let layout = AsyncFrameLayout::try_new(too_many_slots, &TypeMap::new()).unwrap();
    assert!(layout.slot_is_gc_ref(ASYNC_FRAME_GC_SLOT_CAPACITY));
    assert_eq!(layout.gc_payload_bitmap[1], 1);
}

#[test]
fn unit_async_codegen_08_async_function_call_returns_task_type() {
    let function = FunctionDecl {
        requires: Vec::new(),
        ensures: Vec::new(),
        effect_contracts: Vec::new(),
        return_type_span: None,
        type_params: Vec::new(),
        name: "work".to_string(),
        modifiers: Modifiers::ASYNC,
        params: Vec::new(),
        return_type: crate::parser::ast::Type::I64,
        body: Block {
            id: crate::parser::ast::BodyId::fresh(),
            stmts: Vec::new(),
            span: crate::diagnostics::Span::dummy(),
        },
        span: crate::diagnostics::Span::dummy(),
        rust_bridge: None,
        constant: None,
    };

    assert_eq!(
        function_call_return_type(&function),
        Type::Generic("Task".to_string().into(), vec![Type::I64])
    );
}

#[test]
fn unit_async_codegen_10_future_await_runtime_selects_by_output_type() {
    assert_eq!(
        future_await_runtime_name(&Type::Void),
        rt::willow_timer_value_await
    );
    assert_eq!(
        future_await_runtime_name(&Type::I64),
        rt::willow_future_await_i64
    );
    assert_eq!(
        future_await_runtime_name(&Type::Bool),
        rt::willow_future_await_bool
    );
    assert_eq!(
        future_await_runtime_name(&Type::F64),
        rt::willow_future_await_f64
    );
    assert_eq!(
        future_await_runtime_name(&Type::Named("Node".to_string().into())),
        rt::willow_future_await_ptr
    );
}
