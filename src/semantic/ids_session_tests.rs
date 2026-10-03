// Session ownership regression coverage.
use super::*;

fn module() -> SymbolModule {
    SymbolModule::new(
        crate::package::PackageIdentity {
            name: "probe".into(),
            version: "1.0.0".into(),
            source: crate::package::PackageSourceIdentity::Git {
                url: "https://example.test/probe".into(),
            },
            revision: None,
        },
        crate::module::ModulePath("nested::unit".into()),
    )
}

fn panics(f: impl FnOnce()) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err()
}

#[test]
fn owner_borrows_names_and_reuses_indices() {
    let owner = SymbolInterner::new();
    let _scope = owner.enter();
    let ty = TypeId::local("T");
    let function = FunctionId::free("f");
    assert_eq!(ty, TypeId::local("T"));
    assert_eq!(function, FunctionId::free("f"));
    assert_eq!(&*owner.type_name(ty), "T");
    assert_eq!(&*owner.function_name(function), "f");
    assert_eq!(owner.inner.table.borrow().types.len(), 1);
    assert_eq!(owner.inner.table.borrow().functions.len(), 1);
}

#[test]
fn equal_indices_in_different_owners_are_not_equal() {
    let first = SymbolInterner::new();
    let a = {
        let _scope = first.enter();
        TypeId::local("T")
    };
    let second = SymbolInterner::new();
    let b = {
        let _scope = second.enter();
        TypeId::local("T")
    };
    assert_eq!(a.0.index, b.0.index);
    assert_ne!(a, b);
    assert_ne!(a.cmp(&b), std::cmp::Ordering::Equal);
    assert!(panics(|| {
        let _ = second.type_name(a);
    }));
}

#[test]
fn foreign_function_resolution_is_rejected() {
    let first = SymbolInterner::new();
    let id = {
        let _scope = first.enter();
        FunctionId::free("f")
    };
    let second = SymbolInterner::new();
    assert!(panics(|| {
        let _ = second.function_name(id);
    }));
}

#[test]
fn nested_scopes_restore_the_destination() {
    let first = SymbolInterner::new();
    let _scope = first.enter();
    let a = TypeId::local("T");
    let second = SymbolInterner::new();
    {
        let _nested = second.enter();
        assert_ne!(a, TypeId::local("T"));
        assert_eq!(&*first.type_name(a), "T");
    }
    assert_eq!(a, TypeId::local("T"));
}

#[test]
fn panic_restores_outer_owner() {
    let first = SymbolInterner::new();
    let _scope = first.enter();
    let a = TypeId::local("T");
    assert!(panics(|| {
        let second = SymbolInterner::new();
        let _nested = second.enter();
        TypeId::local("only_in_failed_scope");
        panic!("exercise unwind");
    }));
    assert_eq!(a, TypeId::local("T"));
}

#[test]
fn drop_reclaims_owner_strings_and_module_records() {
    let owner = SymbolInterner::new();
    let weak_owner = Rc::downgrade(&owner.inner);
    let (weak_name, weak_module) = {
        let _scope = owner.enter();
        let name = TypeId::local("reclaim_me").name();
        let record = module().spelling();
        (Arc::downgrade(&name), Arc::downgrade(&record))
    };
    let owner_id = owner.inner.id;
    drop(owner);
    assert!(weak_owner.upgrade().is_none());
    assert!(weak_name.upgrade().is_none());
    assert!(weak_module.upgrade().is_none());
    assert!(!OWNERS.with(|owners| owners.borrow().contains_key(&owner_id)));
}

#[test]
fn retained_name_does_not_retain_the_table() {
    let owner = SymbolInterner::new();
    let weak = Rc::downgrade(&owner.inner);
    let name = {
        let _scope = owner.enter();
        TypeId::local("retained").name()
    };
    drop(owner);
    assert!(weak.upgrade().is_none());
    assert_eq!(name.as_ref(), "retained");
}

#[test]
fn stale_handles_cannot_resolve_in_replacement_session() {
    let id = {
        let owner = SymbolInterner::new();
        let _scope = owner.enter();
        TypeId::local("old")
    };
    let replacement = SymbolInterner::new();
    let _scope = replacement.enter();
    assert_eq!(TypeId::local("new").0.index, id.0.index);
    assert!(panics(|| {
        id.name();
    }));
}

#[test]
fn derived_ids_use_their_owner_without_an_active_scope() {
    let owner = SymbolInterner::new();
    let ty = {
        let _scope = owner.enter();
        TypeId::local("T")
    };
    let method = FunctionId::method(ty, "run");
    assert_eq!(method.owner_type(), Some(ty));
    assert_eq!(method.0.owner, owner.inner.id);
    assert_eq!(
        ty.in_namespace("nested").namespace().as_deref(),
        Some("nested")
    );
}

#[test]
fn nested_owner_does_not_capture_derivatives() {
    let owner = SymbolInterner::new();
    let ty = {
        let _scope = owner.enter();
        TypeId::local("T")
    };
    let other = SymbolInterner::new();
    let _scope = other.enter();
    let method = FunctionId::method(ty, "run");
    assert_eq!(method.0.owner, owner.inner.id);
    assert_eq!(other.inner.table.borrow().functions.len(), 0);
}

#[test]
fn foreign_module_attachment_is_rejected() {
    let first = SymbolInterner::new();
    let ty = {
        let _scope = first.enter();
        TypeId::local("T")
    };
    let second = SymbolInterner::new();
    let origin = {
        let _scope = second.enter();
        module()
    };
    assert!(panics(|| {
        ty.in_module(origin);
    }));
}

#[test]
fn module_names_and_aliases_keep_structured_identity() {
    let owner = SymbolInterner::new();
    let _scope = owner.enter();
    let origin = module();
    assert_eq!(origin, module());
    let ty = TypeId::local("T").in_module(origin);
    assert_eq!(ty, ty.in_namespace("consumer_alias"));
    assert_eq!(
        ty,
        TypeId::from_source_name(&format!("{}::T", origin.namespace()))
    );
    assert_eq!(origin.path().0, "nested::unit");
    assert_eq!(origin.package().name, "probe");
}

#[test]
fn artifacts_reintern_into_destination_owner() {
    let first = SymbolInterner::new();
    let (old, json) = {
        let _scope = first.enter();
        let id = FunctionId::method(TypeId::local("T").in_module(module()), "日本");
        (id, serde_json::to_string(&id).unwrap())
    };
    let second = SymbolInterner::new();
    let _scope = second.enter();
    let restored: FunctionId = serde_json::from_str(&json).unwrap();
    assert_ne!(old, restored);
    assert_eq!(restored.0.owner, second.inner.id);
    assert_eq!(serde_json::to_string(&restored).unwrap(), json);
    drop(first);
    assert_eq!(restored.name().as_ref(), "日本");
}

#[test]
fn lazy_artifact_reads_retain_and_select_their_owner() {
    use crate::module::artifacts::UnitArtifacts;
    let first = SymbolInterner::new();
    let weak = Rc::downgrade(&first.inner);
    let (artifacts, record, original) = {
        let _scope = first.enter();
        let artifacts = UnitArtifacts::new().unwrap();
        let id = FunctionId::free("deferred");
        let record = artifacts.write(&id).unwrap();
        (artifacts, record, id)
    };
    drop(first);
    assert!(weak.upgrade().is_some());
    let second = SymbolInterner::new();
    let _scope = second.enter();
    let decoded: FunctionId = artifacts.read(record).unwrap();
    assert_eq!(decoded, original);
    let destination = UnitArtifacts::new().unwrap();
    let copied = destination.copy_record(&artifacts, record).unwrap();
    let imported: FunctionId = destination.read(copied).unwrap();
    assert_ne!(imported, original);
    assert_eq!(imported.0.owner, second.inner.id);
    assert_eq!(decoded.to_string(), imported.to_string());
    drop(artifacts);
    assert!(weak.upgrade().is_none());
}

#[test]
fn lexical_order_does_not_depend_on_allocation_order() {
    for reverse in [false, true] {
        let owner = SymbolInterner::new();
        let _scope = owner.enter();
        let names = if reverse { ["z", "a"] } else { ["a", "z"] };
        let mut ids = names.map(TypeId::local);
        ids.sort();
        assert_eq!(ids.map(|id| id.name().to_string()), ["a", "z"]);
    }
}

#[test]
fn intrinsic_lookup_survives_previous_owner_drop() {
    use crate::semantic::intrinsics::Intrinsic;
    for _ in 0..3 {
        let owner = SymbolInterner::new();
        let _scope = owner.enter();
        for &intrinsic in Intrinsic::ALL {
            assert_eq!(
                Intrinsic::from_function_id(&intrinsic.function_id()),
                Some(intrinsic)
            );
        }
        assert_eq!(
            Intrinsic::from_function_id(&FunctionId::free("ordinary")),
            None
        );
    }
}

#[test]
fn independent_threads_compile_with_independent_owners() {
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|i| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let owner = SymbolInterner::new();
                let _scope = owner.enter();
                barrier.wait();
                for n in 0..128 {
                    let ty = TypeId::local(format!("T_{i}_{n}"));
                    let method = FunctionId::method(ty, "run");
                    assert_eq!(method.owner_type(), Some(ty));
                }
                let fixture = Fixture::new();
                fixture.write("fn main() {} class Concurrent {} ");
                crate::check_file(
                    fixture.path(),
                    &crate::CompilerOptions::debug(),
                    &mut crate::diagnostics::HumanEmitter,
                )
                .unwrap();
                owner.inner.id
            })
        })
        .collect();
    let ids: HashSet<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(ids.len(), 4);
}

#[test]
fn foreign_thread_resolution_fails_without_aliasing() {
    let owner = SymbolInterner::new();
    let _scope = owner.enter();
    let id = TypeId::local("origin");
    assert!(
        std::thread::spawn(move || panics(|| {
            id.name();
        }))
        .join()
        .unwrap()
    );
}

#[test]
fn repeated_requests_have_linear_work_and_unique_storage() {
    for width in [16, 64, 256, 1024] {
        for repetitions in [1, 8, 32] {
            let owner = SymbolInterner::new();
            let _scope = owner.enter();
            for _ in 0..repetitions {
                for n in 0..width {
                    let ty = TypeId::local(format!("T{n}"));
                    FunctionId::method(ty, "run");
                }
            }
            let table = owner.inner.table.borrow();
            assert_eq!(table.requests, 2 * width * repetitions);
            assert_eq!(table.types.len(), width);
            assert_eq!(table.functions.len(), width);
            assert_eq!(table.strings.len(), width + 1);
            eprintln!(
                "width={width} repetitions={repetitions} requests={} strings={}",
                table.requests,
                table.strings.len()
            );
        }
    }
}

#[test]
fn completed_sessions_do_not_accumulate_registry_entries() {
    let before = OWNERS.with(|owners| owners.borrow().len());
    for n in 0..256 {
        let owner = SymbolInterner::new();
        let _scope = owner.enter();
        TypeId::local(format!("session_{n}"));
    }
    assert_eq!(OWNERS.with(|owners| owners.borrow().len()), before);
}

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "willow-symbol-session-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.join("main.wi"))
    }
    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
    fn write(&self, source: &str) {
        std::fs::write(&self.0, source).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(self.0.parent().unwrap()).unwrap();
    }
}

#[test]
fn successful_compiler_session_releases_owner() {
    let fixture = Fixture::new();
    fixture.write("fn main() {} class Success {} ");
    let options = crate::CompilerOptions::debug();
    let session = crate::CompilerSession::new(fixture.path(), "", &options, None);
    let weak = Rc::downgrade(&session.symbols.inner);
    session
        .check_with_emitter(&mut crate::diagnostics::HumanEmitter)
        .unwrap();
    assert!(weak.upgrade().is_none());
}

#[test]
fn failed_compiler_session_releases_owner() {
    let fixture = Fixture::new();
    fixture.write("fn main() { missing(); } class Failure {} ");
    let options = crate::CompilerOptions::debug();
    let session = crate::CompilerSession::new(fixture.path(), "", &options, None);
    let weak = Rc::downgrade(&session.symbols.inner);
    assert!(
        session
            .check_with_emitter(&mut crate::diagnostics::HumanEmitter)
            .is_err()
    );
    assert!(weak.upgrade().is_none());
}

#[test]
fn retained_analysis_revision_owns_symbols_until_drop() {
    let fixture = Fixture::new();
    fixture.write("fn helper() -> i64 { return 7; } fn main() { let n = helper(); } ");
    let options = crate::CompilerOptions::debug();
    let before = OWNERS.with(|owners| owners.borrow().len());
    let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
    for _ in 0..3 {
        let session = crate::CompilerSession::new(fixture.path(), "", &options, None);
        let snapshot = revision
            .analyze(session, &mut crate::diagnostics::HumanEmitter)
            .unwrap();
        assert!(!snapshot.functions.is_empty());
    }
    assert_eq!(OWNERS.with(|owners| owners.borrow().len()), before + 1);
    drop(revision);
    assert_eq!(OWNERS.with(|owners| owners.borrow().len()), before);
}

#[test]
fn hir_and_lir_entry_points_own_and_release_symbols() {
    let fixture = Fixture::new();
    fixture.write("fn main() {} class Emitted {} ");
    let before = OWNERS.with(|owners| owners.borrow().len());
    assert!(!crate::emit_hir_text(fixture.path()).unwrap().is_empty());
    assert!(!crate::emit_lir_text(fixture.path()).unwrap().is_empty());
    assert_eq!(OWNERS.with(|owners| owners.borrow().len()), before);
}
