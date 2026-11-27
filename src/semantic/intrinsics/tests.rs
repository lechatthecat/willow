use super::*;
type Type = crate::parser::ast::Type<String>;

#[test]
fn string_methods_types_arities_and_effects() {
    for (method, arity, result, allocates, panics) in [
        ("len", 0, Type::I64, false, false),
        ("substring", 2, Type::String, true, true),
        ("byte_at", 1, Type::I64, true, true),
        ("slice", 2, Type::String, true, true),
        ("split", 1, Type::Array(Box::new(Type::String)), true, false),
        ("contains", 1, Type::Bool, false, false),
        ("find", 1, generic("Option", vec![Type::I64]), false, false),
        ("ends_with", 1, Type::Bool, false, false),
        ("replace", 2, Type::String, true, true),
        ("trim_start", 0, Type::String, true, false),
        ("trim_end", 0, Type::String, true, false),
        ("to_lower", 0, Type::String, true, false),
        ("to_upper", 0, Type::String, true, false),
        ("trim", 0, Type::String, true, false),
        ("repeat", 1, Type::String, true, true),
        ("starts_with", 1, Type::Bool, false, false),
        ("to_string", 0, Type::String, false, false),
    ] {
        let resolved = resolve(&Type::String, method, arity).unwrap();
        let effects = resolved.intrinsic.runtime_effects();
        assert_eq!(resolved.return_type(|_| None), result);
        assert_eq!(
            effects.contains(RuntimeEffects::MAY_ALLOCATE),
            allocates,
            "{method}"
        );
        assert_eq!(
            effects.contains(RuntimeEffects::MAY_PANIC),
            panics,
            "{method}"
        );
        assert!(resolve(&Type::String, method, arity + 1).is_none());
        if arity > 0 {
            assert!(resolve(&Type::String, method, arity - 1).is_none());
        }
    }
}

fn array(elem: Type) -> Type {
    Type::Array(Box::new(elem))
}

fn generic(name: &str, args: Vec<Type>) -> Type {
    Type::Generic(name.to_string(), args)
}

fn named(name: &str) -> Type {
    Type::Named(name.to_string())
}

fn ret_of(recv: &Type, method: &str, arity: usize) -> Option<Type> {
    resolve(recv, method, arity).map(|r| r.return_type(|_| None))
}

/// Perspective 1: every scalar `to_string` resolves to its own intrinsic, and
/// each produces a String. These four are the whole reason the backend had a
/// `m.method == "to_string"` test ahead of everything else.
#[test]
fn scalar_to_string_resolves_per_receiver() {
    assert_eq!(
        resolve_intrinsic(&Type::I64, "to_string", 0),
        Some(Intrinsic::I64ToString)
    );
    assert_eq!(
        resolve_intrinsic(&Type::F64, "to_string", 0),
        Some(Intrinsic::F64ToString)
    );
    assert_eq!(
        resolve_intrinsic(&Type::Bool, "to_string", 0),
        Some(Intrinsic::BoolToString)
    );
    assert_eq!(
        resolve_intrinsic(&Type::String, "to_string", 0),
        Some(Intrinsic::StringToString)
    );
    for recv in [Type::I64, Type::F64, Type::Bool, Type::String] {
        assert_eq!(ret_of(&recv, "to_string", 0), Some(Type::String));
    }
}

/// Perspective 2: `to_string` with an argument is not the intrinsic. The
/// backend used to guard this with `m.args.is_empty()`; arity is now part of
/// the key, so the guard cannot be forgotten at one call site and kept at
/// another.
#[test]
fn scalar_to_string_with_arguments_does_not_resolve() {
    assert_eq!(resolve(&Type::I64, "to_string", 1), None);
    assert_eq!(resolve(&Type::String, "to_string", 2), None);
}

#[test]
fn array_public_methods_are_source_defined() {
    for (method, arity) in [
        ("len", 0),
        ("push", 1),
        ("pop", 0),
        ("join", 1),
        ("to_string", 0),
        ("freeze", 0),
    ] {
        for element in [Type::I64, Type::String, named("Point")] {
            assert_eq!(resolve(&array(element), method, arity), None);
        }
    }
}

#[test]
fn array_storage_intrinsics_preserve_types_and_arity() {
    assert_eq!(ret_of(&array(Type::I64), "__abi_len", 0), Some(Type::I64));
    assert_eq!(
        ret_of(&array(Type::I64), "__abi_resize", 1),
        Some(Type::Void)
    );
    assert_eq!(
        ret_of(&array(Type::String), "__abi_copy", 0),
        Some(generic("FrozenArray", vec![Type::String]))
    );
    assert_eq!(resolve(&array(Type::I64), "__abi_resize", 0), None);
    assert_eq!(resolve(&array(Type::I64), "__abi_len", 1), None);
    assert_eq!(
        resolve(&Type::FixedArray(Box::new(Type::I64), 2), "__abi_resize", 1),
        None
    );
}

/// Perspective 7: a `FrozenArray<T>` answers `len` only. `push` and `pop`
/// resolve to nothing, so no mutation lowering exists for a frozen array
/// even if a front-end check were weakened.
#[test]
fn frozen_array_exposes_only_len() {
    let fa = generic("FrozenArray", vec![Type::I64]);
    assert_eq!(
        resolve_intrinsic(&fa, "len", 0),
        Some(Intrinsic::FrozenArrayLen)
    );
    assert_eq!(resolve(&fa, "push", 1), None);
    assert_eq!(resolve(&fa, "pop", 0), None);
    assert_eq!(resolve(&fa, "freeze", 0), None);
    assert_eq!(resolve(&fa, "to_string", 0), None);
}

/// Perspective 8: `Map<K, V>::get` produces `Option<V>`, picking the *second*
/// type argument. Swapping the two would type `Map<String, i64>::get` as
/// `Option<String>` and silently coerce a boxed integer.
#[test]
fn map_get_returns_option_of_the_value_type() {
    let map = generic("Map", vec![Type::String, Type::I64]);
    assert_eq!(resolve_intrinsic(&map, "get", 1), Some(Intrinsic::MapGet));
    assert_eq!(
        ret_of(&map, "get", 1),
        Some(generic("Option", vec![Type::I64]))
    );
}

/// Perspective 9: `Map::to_string` produces a String. The backend's
/// structural walk used to answer `void` here, because its `Map` arm ended
/// in a catch-all that swallowed every unlisted method.
#[test]
fn map_to_string_returns_string_not_void() {
    let map = generic("Map", vec![Type::String, Type::I64]);
    assert_eq!(ret_of(&map, "to_string", 0), Some(Type::String));
}

/// Perspective 10: `Map::freeze` preserves both type arguments in order.
#[test]
fn map_freeze_preserves_key_and_value_types() {
    let map = generic("Map", vec![Type::String, Type::Bool]);
    assert_eq!(
        ret_of(&map, "freeze", 0),
        Some(generic("FrozenMap", vec![Type::String, Type::Bool]))
    );
}

/// Perspective 11: a `FrozenMap` is read-only. It shares a runtime
/// representation with `Map`, and the old backend routed both to the same
/// lowering function — which would have emitted `willow_map_insert` on a
/// frozen map had a front-end check ever let one through.
#[test]
fn frozen_map_rejects_mutation_and_refreezing() {
    let fm = generic("FrozenMap", vec![Type::String, Type::I64]);
    assert_eq!(
        resolve_intrinsic(&fm, "get", 1),
        Some(Intrinsic::FrozenMapGet)
    );
    assert_eq!(
        resolve_intrinsic(&fm, "contains", 1),
        Some(Intrinsic::FrozenMapContains)
    );
    assert_eq!(
        resolve_intrinsic(&fm, "len", 0),
        Some(Intrinsic::FrozenMapLen)
    );
    assert_eq!(resolve(&fm, "insert", 2), None);
    assert_eq!(resolve(&fm, "remove", 1), None);
    assert_eq!(resolve(&fm, "freeze", 0), None);
    assert_eq!(resolve(&fm, "to_string", 0), None);
}

/// Perspective 12: `Map` and `FrozenMap` resolve to *distinct* intrinsics
/// even for the reads they share, so a lowering can never accidentally
/// depend on the two being the same value.
#[test]
fn map_and_frozen_map_reads_are_distinct_intrinsics() {
    let map = generic("Map", vec![Type::String, Type::I64]);
    let fm = generic("FrozenMap", vec![Type::String, Type::I64]);
    assert_ne!(
        resolve_intrinsic(&map, "get", 1),
        resolve_intrinsic(&fm, "get", 1)
    );
    assert_ne!(
        resolve_intrinsic(&map, "len", 0),
        resolve_intrinsic(&fm, "len", 0)
    );
}

/// Perspective 13: the collection families are keyed by type-argument count
/// as well as name. `Map<K>` with one argument is not a map.
#[test]
fn collection_families_require_the_right_arity_of_type_arguments() {
    assert_eq!(resolve(&generic("Map", vec![Type::I64]), "len", 0), None);
    assert_eq!(
        resolve(
            &generic("Map", vec![Type::I64, Type::I64, Type::I64]),
            "len",
            0
        ),
        None
    );
    assert_eq!(resolve(&generic("FrozenArray", vec![]), "len", 0), None);
    assert_eq!(
        resolve(&generic("Channel", vec![Type::I64, Type::I64]), "recv", 0),
        None
    );
}

/// Perspective 14: `Channel<T>::recv` produces the element type and `send`
/// produces void. Getting this backwards would make `let v = ch.recv();`
/// bind a void.
#[test]
fn channel_send_and_recv_return_types() {
    let ch = generic("Channel", vec![Type::String]);
    assert_eq!(ret_of(&ch, "recv", 0), Some(Type::String));
    assert_eq!(ret_of(&ch, "send", 1), Some(Type::Void));
    assert_eq!(ret_of(&ch, "close", 0), Some(Type::Void));
    assert_eq!(
        ret_of(&ch, "recv_opt", 0),
        Some(generic("Option", vec![Type::String]))
    );
    assert_eq!(ret_of(&ch, "recv_opt", 1), None);
}

/// Perspective 15: only the two channel operations that can park the task
/// are suspension points. The cooperative passes key off this instead of
/// re-testing the method name.
#[test]
fn only_channel_send_and_recv_suspend() {
    let suspending: Vec<Intrinsic> = Intrinsic::ALL
        .iter()
        .copied()
        .filter(|i| i.is_suspension_point())
        .collect();
    assert_eq!(
        suspending,
        vec![
            Intrinsic::ChannelSend,
            Intrinsic::ChannelRecv,
            Intrinsic::ChannelRecvOpt
        ]
    );
}

/// Perspective 16: `AtomicI64` and `AtomicBool` share intrinsic identities
/// but not result types — `load` gives `i64` on one and `bool` on the other.
#[test]
fn atomic_load_and_swap_follow_the_receiver_width() {
    assert_eq!(ret_of(&named("AtomicI64"), "load", 0), Some(Type::I64));
    assert_eq!(ret_of(&named("AtomicBool"), "load", 0), Some(Type::Bool));
    assert_eq!(ret_of(&named("AtomicI64"), "swap", 1), Some(Type::I64));
    assert_eq!(ret_of(&named("AtomicBool"), "swap", 1), Some(Type::Bool));
    assert_eq!(ret_of(&named("AtomicI64"), "store", 1), Some(Type::Void));
    assert_eq!(ret_of(&named("AtomicBool"), "store", 1), Some(Type::Void));
}

/// Perspective 17: `AtomicBool` has no arithmetic. The lowering builds the
/// runtime symbol name by interpolating the method, so resolving
/// `AtomicBool::add` would have produced a call to the non-existent
/// `willow_atomic_bool_add`.
#[test]
fn atomic_bool_has_no_arithmetic() {
    assert_eq!(
        resolve_intrinsic(&named("AtomicI64"), "add", 1),
        Some(Intrinsic::AtomicAdd)
    );
    assert_eq!(
        resolve_intrinsic(&named("AtomicI64"), "sub", 1),
        Some(Intrinsic::AtomicSub)
    );
    assert_eq!(resolve(&named("AtomicBool"), "add", 1), None);
    assert_eq!(resolve(&named("AtomicBool"), "sub", 1), None);
}

/// Perspective 18: the blocking cells use different method names for the
/// same shape, and neither answers the other's.
#[test]
fn blocking_cells_do_not_share_method_names() {
    let cell = generic("BlockingCell", vec![Type::I64]);
    let rw = generic("BlockingRwCell", vec![Type::I64]);
    assert_eq!(resolve_intrinsic(&cell, "get", 0), Some(Intrinsic::CellGet));
    assert_eq!(resolve_intrinsic(&cell, "set", 1), Some(Intrinsic::CellSet));
    assert_eq!(resolve(&cell, "read", 0), None);
    assert_eq!(resolve(&cell, "write", 1), None);
    assert_eq!(
        resolve_intrinsic(&rw, "read", 0),
        Some(Intrinsic::RwCellRead)
    );
    assert_eq!(
        resolve_intrinsic(&rw, "write", 1),
        Some(Intrinsic::RwCellWrite)
    );
    assert_eq!(resolve(&rw, "get", 0), None);
    assert_eq!(resolve(&rw, "set", 1), None);
}

/// Perspective 19: `Mutex<T>` and `RwLock<T>` have no accessor methods at
/// all — the scheduler-aware locks go through `lock ... as ...`. Resolving
/// one here would bypass that.
#[test]
fn scheduler_locks_expose_no_intrinsic_accessors() {
    for name in ["Mutex", "RwLock"] {
        let lock = generic(name, vec![Type::I64]);
        for method in ["get", "set", "read", "write", "load", "store"] {
            assert_eq!(resolve(&lock, method, 0), None, "{name}::{method}");
            assert_eq!(resolve(&lock, method, 1), None, "{name}::{method}");
        }
    }
}

/// Perspective 20: `Task<T>` and `JoinHandle<T>` are the same frame, so they
/// resolve identically. `result()` yields `TaskResult<T>` — a type the
/// backend's structural walk did not know at all and typed as `i64`.
#[test]
fn task_and_join_handle_resolve_identically() {
    for name in ["Task", "JoinHandle"] {
        let t = generic(name, vec![Type::I64]);
        assert_eq!(
            resolve_intrinsic(&t, "cancel", 0),
            Some(Intrinsic::TaskCancel)
        );
        assert_eq!(
            resolve_intrinsic(&t, "is_cancelled", 0),
            Some(Intrinsic::TaskIsCancelled)
        );
        assert_eq!(
            ret_of(&t, "result", 0),
            Some(generic("TaskResult", vec![Type::I64]))
        );
        assert_eq!(ret_of(&t, "cancel", 0), Some(Type::Void));
        assert_eq!(ret_of(&t, "is_cancelled", 0), Some(Type::Bool));
    }
}

/// Perspective 21: `Future<T>` is awaitable but is not a cancellable handle,
/// so it must not pick up `Task`'s intrinsics.
#[test]
fn future_is_not_a_cancellable_task_handle() {
    let f = generic("Future", vec![Type::I64]);
    assert_eq!(resolve(&f, "cancel", 0), None);
    assert_eq!(resolve(&f, "is_cancelled", 0), None);
    assert_eq!(resolve(&f, "result", 0), None);
}

/// Perspective 22: `TaskResult<T>` is the *output* of `result()`, not
/// another receiver — calling `result()` on it again resolves to nothing
/// rather than nesting.
#[test]
fn task_result_is_not_itself_a_task_handle() {
    let tr = generic("TaskResult", vec![Type::I64]);
    assert_eq!(resolve(&tr, "result", 0), None);
    assert_eq!(resolve(&tr, "cancel", 0), None);
}

/// Perspective 23: the two cancellation handles overlap on three method
/// names and differ on two. `finish` belongs to the scope and `attach` to
/// the token; crossing them is a checker error today and resolves to nothing
/// here.
#[test]
fn token_and_scope_share_names_but_not_intrinsics() {
    let token = named("CancellationToken");
    let scope = named("TaskScope");
    assert_eq!(
        resolve_intrinsic(&token, "cancel", 0),
        Some(Intrinsic::TokenCancel)
    );
    assert_eq!(
        resolve_intrinsic(&scope, "cancel", 0),
        Some(Intrinsic::ScopeCancel)
    );
    assert_ne!(
        resolve_intrinsic(&token, "is_cancelled", 0),
        resolve_intrinsic(&scope, "is_cancelled", 0)
    );
    assert_eq!(
        resolve_intrinsic(&scope, "finish", 0),
        Some(Intrinsic::ScopeFinish)
    );
    assert_eq!(resolve(&token, "finish", 0), None);
    assert_eq!(
        resolve_intrinsic(&token, "attach", 1),
        Some(Intrinsic::TokenAttach)
    );
    assert_eq!(resolve(&scope, "attach", 1), None);
    assert_eq!(
        resolve_intrinsic(&scope, "add", 1),
        Some(Intrinsic::ScopeAdd)
    );
    assert_eq!(resolve(&token, "add", 1), None);
}

/// Perspective 24: `child()` returns the receiver's own handle type, so a
/// token's child is a token and a scope's child is a scope.
#[test]
fn child_returns_the_same_handle_kind() {
    assert_eq!(
        ret_of(&named("CancellationToken"), "child", 0),
        Some(named("CancellationToken"))
    );
    assert_eq!(
        ret_of(&named("TaskScope"), "child", 0),
        Some(named("TaskScope"))
    );
}

/// Perspective 25: `TaskScope::finish` returns the fully spelled
/// `Task<Result<void, Cancelled>>`. This is the checker's answer, restated
/// once so the backend can stop guessing `i64`.
#[test]
fn scope_finish_returns_a_cancellable_unit_task() {
    assert_eq!(
        ret_of(&named("TaskScope"), "finish", 0),
        Some(generic(
            "Task",
            vec![generic("Result", vec![Type::Void, named("Cancelled")])]
        ))
    );
}

/// Perspective 26: `attach`/`add` hand back their argument, so their result
/// type is not knowable from the receiver. The table says so explicitly
/// instead of returning a plausible-looking wrong type.
#[test]
fn attach_and_add_return_their_argument() {
    let attach = resolve(&named("CancellationToken"), "attach", 1).unwrap();
    let add = resolve(&named("TaskScope"), "add", 1).unwrap();
    assert_eq!(attach.ret, IntrinsicReturn::SameAsArg(0));
    assert_eq!(add.ret, IntrinsicReturn::SameAsArg(0));

    let task = generic("Task", vec![Type::String]);
    let seen = attach.return_type(|i| (i == 0).then(|| task.clone()));
    assert_eq!(seen, task);
    assert_eq!(add.return_type(|i| (i == 0).then(|| task.clone())), task);
}

/// Perspective 27: with no argument type available, the argument-typed
/// intrinsics fall back to `Task<void>` rather than to a scalar. A caller
/// that cannot type its arguments still gets a task-shaped answer, so a
/// downstream `await` sees a handle instead of an integer.
#[test]
fn attach_and_add_fall_back_to_a_task_shape() {
    let add = resolve(&named("TaskScope"), "add", 1).unwrap();
    assert_eq!(add.return_type(|_| None), generic("Task", vec![Type::Void]));
}

/// Perspective 28: an ordinary user class receiver resolves to nothing, even
/// when it spells a builtin method name. Class dispatch must stay reachable.
#[test]
fn user_class_receivers_never_resolve() {
    let user = named("Point");
    for method in [
        "len",
        "push",
        "pop",
        "get",
        "set",
        "send",
        "recv",
        "recv_opt",
        "close",
        "cancel",
        "to_string",
        "freeze",
        "child",
        "finish",
    ] {
        for arity in 0..3 {
            assert_eq!(resolve(&user, method, arity), None, "Point::{method}");
        }
    }
}

/// Perspective 29: a user *generic* class receiver resolves to nothing too,
/// as long as it is not one of the reserved family names.
#[test]
fn user_generic_receivers_never_resolve() {
    let user = generic("Stack", vec![Type::I64]);
    for method in ["len", "push", "pop", "recv", "to_string"] {
        for arity in 0..3 {
            assert_eq!(resolve(&user, method, arity), None, "Stack::{method}");
        }
    }
}

/// Perspective 30: unknown method names on a known family resolve to
/// nothing, so a misspelling reaches the checker's diagnostic rather than a
/// lowering for a neighbouring method.
#[test]
fn unknown_methods_on_known_families_do_not_resolve() {
    assert_eq!(resolve(&array(Type::I64), "lenght", 0), None);
    assert_eq!(
        resolve(&generic("Map", vec![Type::String, Type::I64]), "put", 2),
        None
    );
    assert_eq!(
        resolve(&generic("Channel", vec![Type::I64]), "receive", 0),
        None
    );
    assert_eq!(resolve(&named("AtomicI64"), "fetch_add", 1), None);
}

/// Perspective 31: `void` and `!` receivers resolve to nothing. Both appear
/// in compiler-synthesized expressions, and neither has methods.
#[test]
fn uninhabited_and_unit_receivers_do_not_resolve() {
    for recv in [Type::Void, Type::Never] {
        for method in ["to_string", "len", "recv"] {
            assert_eq!(resolve(&recv, method, 0), None);
        }
    }
}

/// Perspective 32: an empty method name resolves to nothing rather than
/// panicking or matching a prefix.
#[test]
fn empty_method_name_does_not_resolve() {
    assert_eq!(resolve(&array(Type::I64), "", 0), None);
    assert_eq!(resolve(&Type::I64, "", 0), None);
}

/// Perspective 33: nested receivers resolve on their outermost family, so
/// Array copy retains the nested element rather than flattening it.
#[test]
fn nested_receivers_resolve_on_the_outer_family() {
    let nested = array(array(Type::I64));
    assert_eq!(
        ret_of(&nested, "__abi_copy", 0),
        Some(generic("FrozenArray", vec![array(Type::I64)]))
    );
    assert_eq!(ret_of(&nested, "__abi_len", 0), Some(Type::I64));

    let map_of_arrays = generic("Map", vec![Type::String, array(Type::I64)]);
    assert_eq!(
        ret_of(&map_of_arrays, "get", 1),
        Some(generic("Option", vec![array(Type::I64)]))
    );
}

/// Perspective 34: every variant in [`Intrinsic::ALL`] is actually
/// reachable from `resolve`. A variant nobody can produce is either dead or
/// a lowering waiting for a resolution that never happens.
#[test]
fn every_intrinsic_is_reachable_from_resolve() {
    let receivers: Vec<Type> = vec![
        Type::U64,
        Type::Char,
        Type::U8,
        Type::I64,
        Type::F64,
        Type::Bool,
        Type::String,
        array(Type::I64),
        array(Type::String),
        generic("FrozenArray", vec![Type::I64]),
        generic("Map", vec![Type::String, Type::I64]),
        generic("FrozenMap", vec![Type::String, Type::I64]),
        generic("Channel", vec![Type::I64]),
        generic("BlockingCell", vec![Type::I64]),
        generic("BlockingRwCell", vec![Type::I64]),
        generic("Task", vec![Type::I64]),
        generic("JoinHandle", vec![Type::I64]),
        named("AtomicI64"),
        named("AtomicBool"),
        named("CancellationToken"),
        named("TaskScope"),
    ];
    let methods = [
        "parse_i8",
        "to_i64",
        "wrapping_to_u8",
        "checked_to_u8",
        "__abi_len",
        "__abi_resize",
        "__abi_copy",
        "__abi_concat",
        "__abi_debug",
        "join",
        "byte_at",
        "substring",
        "slice",
        "split",
        "find",
        "trim",
        "repeat",
        "ends_with",
        "replace",
        "trim_start",
        "trim_end",
        "to_lower",
        "to_upper",
        "starts_with",
        "to_string",
        "len",
        "push",
        "pop",
        "freeze",
        "remove",
        "keys",
        "values",
        "entries",
        "insert",
        "get",
        "set",
        "contains",
        "read",
        "write",
        "send",
        "recv",
        "recv_opt",
        "close",
        "load",
        "store",
        "swap",
        "compare_exchange",
        "add",
        "sub",
        "cancel",
        "is_cancelled",
        "result",
        "child",
        "attach",
        "finish",
        "wrapping_add",
        "wrapping_sub",
        "wrapping_mul",
        "wrapping_neg",
        "checked_add",
        "checked_sub",
        "checked_mul",
        "checked_neg",
    ];
    let mut seen = std::collections::HashSet::new();
    for recv in &receivers {
        for method in methods {
            for arity in 0..3 {
                if let Some(r) = resolve(recv, method, arity) {
                    let id = r.intrinsic;
                    let args = vec![Type::I64; arity];
                    let result = r.return_type(|i| args.get(i).cloned());
                    let name = MethodName::classify(method);
                    for &candidate in Intrinsic::ALL {
                        assert_eq!(
                            matches_signature(recv, name, candidate, &args, &result),
                            candidate == id,
                            "{recv:?}.{method} with {candidate:?}"
                        );
                    }
                    seen.insert(id);
                }
            }
        }
    }
    let missing: Vec<Intrinsic> = Intrinsic::ALL
        .iter()
        .copied()
        .filter(|i| !seen.contains(i))
        .collect();
    assert!(missing.is_empty(), "unreachable intrinsics: {missing:?}");
}

/// Perspective 35: [`Intrinsic::ALL`] really is every variant. Counting is
/// the only check Rust cannot do for us here, so it is spelled out — a new
/// variant left out of `ALL` would silently escape the exhaustiveness tests
/// that iterate it.
#[test]
fn all_variants_are_listed() {
    let unique: std::collections::HashSet<Intrinsic> = Intrinsic::ALL.iter().copied().collect();
    assert_eq!(unique.len(), Intrinsic::ALL.len(), "duplicate in ALL");
    assert_eq!(
        Intrinsic::ALL.len(),
        77,
        "Intrinsic::ALL must list every variant; update the count when adding one"
    );
}

/// Perspective 36: resolution is deterministic and free of ordering effects
/// — the same key always gives the same answer, and no receiver resolves two
/// different intrinsics for one method/arity pair.
#[test]
fn resolution_is_deterministic() {
    let map = generic("Map", vec![Type::String, Type::I64]);
    for _ in 0..4 {
        assert_eq!(resolve_intrinsic(&map, "get", 1), Some(Intrinsic::MapGet));
    }
    assert_eq!(resolve(&map, "get", 1), resolve(&map, "get", 1));
}
