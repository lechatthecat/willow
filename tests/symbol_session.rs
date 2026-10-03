//! Exercise the production library: no unit-test implicit arena is available.
use willow_compiler::semantic::ids::{FunctionId, SymbolInterner, TypeId};

#[test]
fn standalone_symbol_construction_requires_explicit_owner() {
    assert!(std::panic::catch_unwind(|| TypeId::local("no_owner")).is_err());
    let owner = SymbolInterner::new();
    assert_eq!(&*owner.type_name(owner.intern_type("explicit")), "explicit");
    assert_eq!(&*owner.function_name(owner.intern_function("f")), "f");
    {
        let _scope = owner.enter();
        assert_eq!(&*owner.type_name(TypeId::local("explicit")), "explicit");
    }
    assert!(std::panic::catch_unwind(|| FunctionId::free("no_owner")).is_err());
}

#[test]
fn structured_artifacts_cross_owner_and_thread_boundaries() {
    let json = std::thread::spawn(|| {
        let owner = SymbolInterner::new();
        let _scope = owner.enter();
        serde_json::to_string(&FunctionId::method(TypeId::local("日本"), "run")).unwrap()
    })
    .join()
    .unwrap();
    let owner = SymbolInterner::new();
    let _scope = owner.enter();
    let id: FunctionId = serde_json::from_str(&json).unwrap();
    assert_eq!(id.owner().as_deref(), Some("日本"));
    assert_eq!(&*owner.function_name(id), "run");
    assert_eq!(serde_json::to_string(&id).unwrap(), json);
}
