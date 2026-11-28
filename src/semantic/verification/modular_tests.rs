use super::*;
use std::{fs, path::PathBuf};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-modular-proof-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
    fn write(&self, name: &str, source: &str) {
        fs::write(self.0.join(name), source).unwrap();
    }
    fn check(&self, previous: Option<&crate::Frontend>) -> crate::Frontend {
        let source = fs::read_to_string(self.0.join("main.wi")).unwrap();
        let map =
            crate::diagnostics::SourceMap::new(self.0.join("main.wi").to_str().unwrap(), &source);
        let mut inputs = crate::compiler_db::inputs::CompilerInputs::native(
            crate::CompilerOptions::debug(),
            self.0.clone(),
        );
        inputs.verification = true;
        crate::run_frontend_revision(
            &source,
            &self.0,
            &map,
            inputs,
            &mut crate::diagnostics::HumanEmitter,
            previous,
            None,
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const IMPORT_CONTRACT_CASES: [(&str, &str, &str, &str, &str, Status); 22] = [
    (
        "local module constant",
        "const LIMIT:i64=100; @pure @verified pub fn id(x:i64)->i64 requires x<LIMIT ensures result==x {return x;}",
        "import math;",
        "requires x<100 ensures result==x",
        "math::id(x)",
        Status::Proven,
    ),
    (
        "impure declaration",
        "@pure pub fn id(x:i64)->i64 {println(x);return x;}",
        "import math;",
        "ensures math::id(result)==x",
        "x",
        Status::Unknown,
    ),
    (
        "module call",
        "@pure @verified pub fn id(x:i64)->i64 ensures result==x {return x;}",
        "import math;",
        "ensures result==x",
        "math::id(x)",
        Status::Proven,
    ),
    (
        "module alias",
        "@pure @verified pub fn id(x:i64)->i64 ensures result==x {return x;}",
        "import math as m;",
        "ensures result==x",
        "m::id(x)",
        Status::Proven,
    ),
    (
        "item alias",
        "@pure @verified pub fn id(x:i64)->i64 ensures result==x {return x;}",
        "import math::id as identity;",
        "ensures result==x",
        "identity(x)",
        Status::Proven,
    ),
    (
        "requires satisfied",
        "@pure @verified pub fn id(x:i64)->i64 requires x>=0 ensures result==x {return x;}",
        "import math;",
        "requires x>=0 ensures result==x",
        "math::id(x)",
        Status::Proven,
    ),
    (
        "requires counterexample",
        "@pure @verified pub fn id(x:i64)->i64 requires x>=0 ensures result==x {return x;}",
        "import math;",
        "",
        "math::id(x)",
        Status::Counterexample,
    ),
    (
        "weak contract hides body",
        "@pure @verified pub fn id(x:i64)->i64 ensures result>=x {return x;}",
        "import math;",
        "ensures result==x",
        "math::id(x)",
        Status::Counterexample,
    ),
    (
        "pure requires predicate",
        "@pure pub fn positive(x:i64)->bool {return x>=0;}",
        "import math;",
        "requires math::positive(x) ensures result>=0",
        "x",
        Status::Proven,
    ),
    (
        "pure ensures predicate",
        "@pure pub fn positive(x:i64)->bool {return x>=0;}",
        "import math;",
        "requires x>=0 ensures math::positive(result)",
        "x",
        Status::Proven,
    ),
    (
        "helper guard counterexample",
        "@pure pub fn id(x:i64)->i64 requires x>0 {return x;}",
        "import math;",
        "ensures math::id(result)==x",
        "x",
        Status::Counterexample,
    ),
    (
        "helper overflow counterexample",
        "@pure pub fn id(x:i64)->i64 {return x+1;}",
        "import math;",
        "ensures math::id(result)>x",
        "x",
        Status::Counterexample,
    ),
    (
        "helper runtime ensures",
        "@pure pub fn id(x:i64)->i64 ensures result>x {return x;}",
        "import math;",
        "ensures math::id(result)==x",
        "x",
        Status::Counterexample,
    ),
    (
        "short circuit guard",
        "@pure pub fn id(x:i64)->i64 requires x>0 {return x;}",
        "import math;",
        "ensures result<=0 || math::id(result)==x",
        "x",
        Status::Proven,
    ),
    (
        "recursive helper",
        "@pure pub fn id(x:i64)->i64 {return id(x);}",
        "import math;",
        "ensures math::id(result)==x",
        "x",
        Status::Unknown,
    ),
    (
        "recursive verified",
        "@pure @verified pub fn id(x:i64)->i64 {return id(x);}",
        "import math;",
        "",
        "math::id(x)",
        Status::Unknown,
    ),
    (
        "unsupported helper loop",
        "@pure pub fn id(x:i64)->i64 {while false {} return x;}",
        "import math;",
        "ensures math::id(result)==x",
        "x",
        Status::Unknown,
    ),
    (
        "nested pure helpers",
        "@pure fn inner(x:i64)->bool {return x>=0;} @pure pub fn positive(x:i64)->bool {return inner(x);}",
        "import math;",
        "requires math::positive(x) ensures result>=0",
        "x",
        Status::Proven,
    ),
    (
        "boolean return",
        "@pure @verified pub fn positive(x:i64)->bool ensures result==(x>=0) {return x>=0;}",
        "import math;",
        "requires math::positive(x) ensures result>=0",
        "x",
        Status::Proven,
    ),
    (
        "verified in postcondition",
        "@pure @verified pub fn id(x:i64)->i64 ensures result==x {return x;}",
        "import math;",
        "ensures math::id(result)==x",
        "x",
        Status::Proven,
    ),
    (
        "caller argument overflow",
        "@pure @verified pub fn id(x:i64)->i64 ensures result==x {return x;}",
        "import math;",
        "",
        "math::id(x+1)",
        Status::Counterexample,
    ),
    (
        "contract dependency of callee",
        "@pure fn positive(x:i64)->bool {return x>=0;} @pure @verified pub fn id(x:i64)->i64 requires positive(x) ensures positive(result) {return x;}",
        "import math;",
        "requires x>=0 ensures result>=0",
        "math::id(x)",
        Status::Proven,
    ),
];
#[test]
fn modular_proof_import_and_contract_perspectives() {
    for (label, module, imports, contracts, body, want) in IMPORT_CONTRACT_CASES {
        eprintln!("perspective: {label}");
        let fixture = Fixture::new();
        fixture.write("math.wi", module);
        fixture.write("main.wi", &format!("{imports} @pure @verified fn caller(x:i64)->i64 {contracts} {{return {body};}} fn main() {{}}"));
        let frontend = fixture.check(None);
        let world = frontend.db.effects.proofs.borrow();
        let export = world.exports.get("caller").expect("caller export");
        assert_eq!(
            export.error.is_none(),
            want == Status::Proven,
            "{label}: {:?}",
            export.error
        );
        let diagnostics = frontend
            .db
            .unit_diagnostics(crate::module::UnitId::ENTRY)
            .unwrap();
        if want == Status::Counterexample {
            assert!(
                diagnostics
                    .iter()
                    .any(|d| d.code == ErrorCode::E2630 && d.message.contains("candidate inputs")),
                "{label}: {diagnostics:?}"
            );
        }
        if want == Status::Unknown {
            assert!(
                diagnostics
                    .iter()
                    // An impure contract callee leaves the caller's implied purity UNKNOWN.
                    .any(|d| matches!(d.code, ErrorCode::E2631 | ErrorCode::E2632)
                        || (d.code == ErrorCode::E2620 && d.message.contains("UNKNOWN"))),
                "{label}: {diagnostics:?}"
            );
        }
    }
}

#[test]
fn modular_proof_cache_rechecks_only_affected_functions() {
    for size in [4, 8, 16] {
        let f = Fixture::new();
        let leaf =
            "@pure @verified pub fn id(x:i64)->i64 requires x>=0 ensures result==x {return x;}";
        f.write("leaf.wi", leaf);
        let mut entry = String::from("import leaf;\n");
        for i in 0..size {
            f.write(&format!("user{i}.wi"), "import leaf; @pure @verified pub fn use_id(x:i64)->i64 requires x>=1 ensures result==x {return leaf::id(x);}");
            f.write(
                &format!("other{i}.wi"),
                "@pure @verified pub fn identity(x:i64)->i64 ensures result==x {return x;}",
            );
            entry += &format!("import user{i}; import other{i};\n");
        }
        entry += "fn main() {}";
        f.write("main.wi", &entry);
        PROOF_RUNS.set(0);
        let first = f.check(None);
        assert_eq!(PROOF_RUNS.get(), 2 * size + 1);
        PROOF_RUNS.set(0);
        let same = f.check(Some(&first));
        assert_eq!(PROOF_RUNS.get(), 0);
        f.write("leaf.wi", &leaf.replace("x>=0", "x>=1"));
        PROOF_RUNS.set(0);
        let changed = f.check(Some(&same));
        assert_eq!(PROOF_RUNS.get(), size + 1);
        assert!(
            changed
                .db
                .unit_diagnostics(crate::module::UnitId::ENTRY)
                .unwrap()
                .is_empty()
        );
        println!(
            "modular fanout modules={} call_edges={size} cold={} unchanged=0 edited={} unrelated_reproved=0",
            2 * size + 2,
            2 * size + 1,
            PROOF_RUNS.get()
        );
    }
}

#[test]
fn modular_proof_body_changes_stop_at_proven_contract_boundary() {
    let f = Fixture::new();
    f.write(
        "leaf.wi",
        "@pure @verified pub fn id(x:i64)->i64 ensures result==x {return x;}",
    );
    f.write("main.wi", "import leaf; @pure @verified fn caller(x:i64)->i64 ensures result==x {return leaf::id(x);} fn main() {}");
    let first = f.check(None);
    f.write(
        "leaf.wi",
        "@pure @verified pub fn id(x:i64)->i64 ensures result==x {if x==0 {return 0;} return x;}",
    );
    PROOF_RUNS.set(0);
    let changed = f.check(Some(&first));
    assert_eq!(PROOF_RUNS.get(), 1);
    assert_eq!(changed.db.effects.proofs.borrow().cache.hits, 1);
}

#[test]
fn modular_proof_failed_dependency_does_not_evict_unrelated_proofs() {
    let f = Fixture::new();
    f.write(
        "leaf.wi",
        "@pure @verified pub fn id(x:i64)->i64 ensures result==x {return x;}",
    );
    f.write(
        "other.wi",
        "@pure @verified pub fn other(x:i64)->i64 ensures result==x {return x;}",
    );
    f.write("main.wi", "import leaf; import other; @pure @verified fn caller(x:i64)->i64 ensures result==x {return leaf::id(x);} fn main() {}");
    let first = f.check(None);
    f.write(
        "leaf.wi",
        "@pure @verified pub fn id(x:i64)->i64 ensures result>x {return x;}",
    );
    PROOF_RUNS.set(0);
    let changed = f.check(Some(&first));
    assert_eq!(PROOF_RUNS.get(), 1);
    let proofs = changed.db.effects.proofs.borrow();
    assert!(proofs.exports["caller"].error.is_some());
    assert_eq!(proofs.cache.hits, 1);
}

#[test]
fn modular_proof_local_contract_helpers_and_same_named_modules() {
    let f = Fixture::new();
    f.write("left.wi", "@pure pub fn id(x:i64)->i64 {return x;}");
    f.write("right.wi", "@pure pub fn id(x:i64)->i64 {return 0;}");
    f.write("main.wi", "import left; import right; @pure fn nonnegative(x:i64)->bool {return x>=0;} @pure @verified fn caller(x:i64)->i64 requires nonnegative(x) ensures left::id(result)==x && right::id(result)==0 {return x;} fn main() {}");
    let checked = f.check(None);
    assert!(
        checked.db.effects.proofs.borrow().exports["caller"]
            .error
            .is_none()
    );
}

#[test]
fn modular_proof_chain_operation_counts_stop_at_contracts() {
    for n in [8, 16, 32] {
        let f = Fixture::new();
        let leaf =
            "@pure @verified pub fn id(x:i64)->i64 requires x>=0 ensures result==x {return x;}";
        f.write("m0.wi", leaf);
        for i in 1..n {
            f.write(&format!("m{i}.wi"),&format!("import m{}; @pure @verified pub fn id(x:i64)->i64 requires x>=1 ensures result==x {{return m{}::id(x);}}",i-1,i-1));
        }
        f.write(
            "main.wi",
            &format!(
                "import m{}; fn main() {{println(m{}::id(1));}}",
                n - 1,
                n - 1
            ),
        );
        PROOF_RUNS.set(0);
        let first = f.check(None);
        assert_eq!(PROOF_RUNS.get(), n);
        assert_eq!(first.db.effects.proofs.borrow().import_visits.get(), n - 1);
        f.write("m0.wi", &leaf.replace("x>=0", "x>=1"));
        PROOF_RUNS.set(0);
        let changed = f.check(Some(&first));
        assert_eq!(PROOF_RUNS.get(), 2);
        assert_eq!(
            changed.db.effects.proofs.borrow().import_visits.get(),
            n - 1
        );
        println!(
            "modular chain modules={} call_edges={} import_summary_visits={} cold={n} edited=2",
            n + 1,
            n - 1,
            n - 1
        );
    }
}

#[test]
fn modular_proof_cache_survives_declaration_reordering_and_large_inventory() {
    use super::super::tests::{options, parse};
    let source = (0..270)
        .map(|i| {
            format!("@pure @verified fn sample{i}(x:i64)->i64 ensures result==x {{return x;}}\n")
        })
        .collect::<String>();
    let mut cache = ProofCache::default();
    let first = verify_cached(&parse(&source), &options(), false, &mut cache);
    assert!(first.reports.iter().all(|r| r.status == Status::Proven));
    let reordered = source.lines().rev().collect::<Vec<_>>().join("\n");
    PROOF_RUNS.set(0);
    let same = verify_cached(&parse(&reordered), &options(), false, &mut cache);
    assert!(same.reports.iter().all(|r| r.status == Status::Proven));
    assert_eq!(PROOF_RUNS.get(), 0);
    assert_eq!(cache.hits, 270);
}

#[test]
fn modular_proof_dependency_reordering_preserves_cache_and_exports() {
    for n in [4, 8, 16] {
        let f = Fixture::new();
        let declarations: Vec<_> = (0..n)
            .map(|i| format!("@pure fn p{i}(x:i64)->bool {{return x>={i};}}\n"))
            .collect();
        let condition = (0..n)
            .map(|i| format!("p{i}(x)"))
            .collect::<Vec<_>>()
            .join(" && ");
        // Exercise both full-body pure exports and contract-only verified exports.
        let users = format!(
            "@pure pub fn all(x:i64)->bool {{return {condition};}} @pure @verified pub fn opaque(x:i64)->bool {{return all(x);}} @pure @verified pub fn guarded(x:i64)->i64 requires {condition} ensures result==x {{return x;}}"
        );
        f.write("math.wi", &(declarations.concat() + &users));
        f.write("main.wi", "import math; @pure @verified fn caller(x:i64)->i64 requires math::opaque(x) ensures result==x {return x;} @pure @verified fn guarded_caller(x:i64)->i64 requires math::all(x) ensures result==x {return math::guarded(x);} fn main() {}");
        PROOF_RUNS.set(0);
        let first = f.check(None);
        assert_eq!(PROOF_RUNS.get(), 4);
        f.write(
            "math.wi",
            &(declarations.iter().rev().cloned().collect::<String>() + &users),
        );
        PROOF_RUNS.set(0);
        let reordered = f.check(Some(&first));
        assert_eq!(PROOF_RUNS.get(), 0, "{n} dependencies");
        let old = first.db.effects.proofs.borrow();
        let new = reordered.db.effects.proofs.borrow();
        assert_eq!(new.cache.hits, 4);
        assert!(new.reports.iter().all(|r| r.status == Status::Proven));
        for (name, export) in &old.exports {
            assert_eq!(new.exports[name].digest, export.digest, "{name}");
            assert_eq!(
                new.exports[name].dependencies, export.dependencies,
                "{name}"
            );
        }
        for report in &new.reports {
            let before = old
                .reports
                .iter()
                .find(|r| r.function == report.function)
                .unwrap();
            assert_eq!(report.dependencies, before.dependencies);
            assert_eq!(report.dependency_digests, before.dependency_digests);
        }
        println!("dependency_reordering dependencies={n} cold=4 reordered=0 cache_hits=4");
    }
}
