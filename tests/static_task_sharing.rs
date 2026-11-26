//! End-to-end task reachability, structural Sync, source labels and cached checking.
use std::{
    fs, io,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
use willow_compiler::{
    CompilerOptions, check_file,
    diagnostics::{Diagnostic, DiagnosticEmitter, ErrorCode, Severity, source_map::SourceLookup},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-static-sharing-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        for (name, source) in files {
            fs::write(path.join(name), source).unwrap();
        }
        Self(path)
    }
    fn check(&self) -> (bool, Collected) {
        let mut collected = Collected::default();
        let ok = check_file(
            self.0.join("main.wi").to_str().unwrap(),
            &CompilerOptions::debug(),
            &mut collected,
        )
        .is_ok();
        (ok, collected)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[derive(Default, Debug)]
struct Collected {
    diagnostics: Vec<Diagnostic>,
    labels: Vec<(String, String)>,
}
impl DiagnosticEmitter for Collected {
    fn emit(&mut self, diagnostic: &Diagnostic, sources: &dyn SourceLookup) -> io::Result<()> {
        if diagnostic.code == ErrorCode::E2402 && diagnostic.message.contains("cannot share static")
        {
            assert_eq!(diagnostic.labels.len(), 2, "{diagnostic:?}");
            for label in &diagnostic.labels {
                let source = sources
                    .get(label.span.file_id)
                    .expect("static diagnostic source");
                self.labels.push((
                    source.path.clone(),
                    source.source[label.span.start..label.span.end].to_owned(),
                ));
            }
        }
        self.diagnostics.push(diagnostic.clone());
        Ok(())
    }
}
const ARRAY: &str =
    "import std::collections::Array; class G { pub static data: Array<i64> = [1]; }";
fn sharing_errors(result: &Collected) -> usize {
    result
        .diagnostics
        .iter()
        .filter(|d| d.code == ErrorCode::E2402 && d.message.contains("cannot share static"))
        .count()
}
fn rejected(files: &[(&str, &str)]) {
    let fixture = Fixture::new(files);
    // Repeated rejection must preserve obligations and source labels.
    // Successful revisions in the cache tests below exercise persistent replay.
    for _ in 0..2 {
        let (ok, result) = fixture.check();
        assert!(!ok && sharing_errors(&result) > 0, "{files:?}\n{result:?}");
        assert!(
            result
                .diagnostics
                .iter()
                .all(|d| d.severity != Severity::Error || d.code == ErrorCode::E2402),
            "{files:?}\n{result:?}"
        );
    }
}
fn accepted(source: &str) {
    let fixture = Fixture::new(&[("main.wi", source)]);
    let (ok, result) = fixture.check();
    assert!(ok, "{source}\n{result:?}");
}

#[test]
fn static_task_sharing_rejects_access_paths() {
    for body in [
        "async fn main() { G::data.push(2); }",
        "async fn main() { println(G::data[0]); }",
        "async fn main() { G::data[0] = 2; }",
        "async fn main() { let alias = G::data; println(alias.len()); }",
        "fn helper() { G::data.push(2); } async fn main() { helper(); }",
        "fn leaf() { G::data.push(2); } fn middle() { leaf(); } async fn main() { middle(); }",
        "fn helper() { G::data.push(2); } async fn main() { let callback = helper; callback(); }",
        "async fn main() { let callback = || { G::data.push(2); }; callback(); }",
        "fn invoke(f: fn()) { f(); } fn helper() { G::data.push(2); } async fn main() { invoke(helper); }",
        "class Helper { pub static fn run() { G::data.push(2); } } async fn main() { Helper::run(); }",
        "class Helper { pub fn run() { G::data.push(2); } } async fn main() { new Helper().run(); }",
        "class Helper { pub init(self) { G::data.push(2); } } async fn main() { let h = new Helper(); }",
        "interface Run { fn run(); } class Helper implements Run { pub fn run() { G::data.push(2); } } fn invoke(h: Run) { h.run(); } async fn main() { invoke(new Helper()); }",
        "interface Run { fn run(self) { G::data.push(2); } } class Helper implements Run {} async fn main() { new Helper().run(); }",
        "fn helper<T>(v: T) { G::data.push(2); } async fn main() { helper(1); }",
        "async fn main() { defer { G::data.push(2); } }",
        "open class Base { pub open fn run() {} } class Helper extends Base { pub override fn run() { G::data.push(2); } } fn invoke(h: Base) { h.run(); } async fn main() { invoke(new Helper()); }",
    ] {
        rejected(&[("main.wi", &format!("{ARRAY} {body}"))]);
    }
}

#[test]
fn static_task_sharing_rejects_storage_shapes() {
    for source in [
        "class Counter { pub n: i64; } class G { pub static c: Counter = new Counter(0); } async fn work() { for i in 0..2000000 { G::c.n = G::c.n + 1; } } async fn main() { let a = work(); let b = work(); await a; await b; println(G::c.n); }",
        "import std::collections::Map; class G { pub static data: Map<String, i64> = Map::new(); } async fn main() { G::data.insert(\"x\", 1); }",
        "fn make() -> fn() -> i64 { let n = 1; return || n; } class G { pub static cb: fn() -> i64 = make(); } async fn main() { let cb = G::cb; println(cb()); }",
        "interface Value { fn get() -> i64; } class Box implements Value { pub fn get() -> i64 { return 1; } } class G { pub static value: Value = new Box(); } async fn main() { println(G::value.get()); }",
        "import std::collections::Array; class Box<T> { pub value: T; } class G { pub static data: Box<Array<i64>> = new Box([1]); } async fn main() { println(G::data.value.len()); }",
        "import std::collections::Array; open class Base { pub static data: Array<i64> = [1]; } class Child extends Base {} async fn main() { println(Child::data.len()); }",
        "import std::collections::Array; class G { pub static data: Option<Array<i64>> = Option::Some([1]); } async fn main() { match G::data { Option::Some(a) => { a.push(2); } Option::None => {} } }",
        "class Counter { pub n: i64; } class G { pub static data: FrozenArray<Counter> = [new Counter(0)].freeze(); } fn mutate(c: Counter) { c.n = 1; } async fn main() { println(G::data.len()); }",
    ] {
        rejected(&[("main.wi", source)]);
    }
}

#[test]
fn static_task_sharing_accepts_sync_values_and_sync_only_paths() {
    for source in [
        "class G { pub static value: i64 = 42; } async fn main() { println(G::value); }",
        "class G { pub static value: String = \"hello\"; } async fn main() { println(G::value); }",
        "class Box { pub n: i64; } class G { pub static value: Box = new Box(42); } async fn main() { println(G::value.n); }",
        "class G { pub static value: AtomicI64 = AtomicI64::new(0); } async fn main() { G::value.add(1); }",
        "class G { pub static value: AtomicBool = AtomicBool::new(false); } async fn main() { G::value.store(true); }",
        "import std::collections::Array; class G { pub static value: Mutex<Array<i64>> = Mutex::new([1]); } async fn main() { lock G::value as mut a { a.push(2); } }",
        "class G { pub static value: RwLock<i64> = RwLock::new(1); } async fn main() { lock write G::value as mut a { a = 2; } }",
        "class G { pub static value: FrozenArray<i64> = [1].freeze(); } async fn main() { println(G::value[0]); }",
        "import std::collections::Map; fn make() -> FrozenMap<String, i64> { let m: Map<String, i64> = Map::new(); m.insert(\"x\", 1); return m.freeze(); } class G { pub static value: FrozenMap<String, i64> = make(); } async fn main() { println(G::value.len()); }",
        "class G { pub static value: Option<i64> = Option::Some(1); } async fn main() { println(G::value.is_some()); }",
        "import std::collections::Array; class G { pub static data: Array<i64> = [1]; } fn main() { G::data.push(2); }",
        "import std::collections::Array; class G { pub static data: Array<i64> = [1]; } fn unused() { G::data.push(2); } async fn main() {}",
        "import std::collections::Array; class G { pub static data: Array<i64> = [1]; } fn seed() -> i64 { return G::data.len(); } class H { pub static count: i64 = seed(); } async fn main() { println(H::count); }",
        "class G { pub static mut value: i64 = 0; } async fn main() { G::value = 1; }",
    ] {
        accepted(source);
    }
}

#[test]
fn static_task_sharing_imports_and_declaration_labels() {
    let dep = "module dep; import std::collections::Array; pub class G { pub static data: Array<i64> = [1]; } pub fn helper() { G::data.push(2); }";
    for source in [
        "import dep; async fn main() { dep::G::data.push(2); }",
        "import dep::G; async fn main() { G::data.push(2); }",
        "import dep::G as Renamed; async fn main() { Renamed::data.push(2); }",
        "import dep; async fn main() { dep::helper(); }",
        "import dep::helper; async fn main() { helper(); }",
    ] {
        rejected(&[("dep.wi", dep), ("main.wi", source)]);
    }
    let fixture = Fixture::new(&[
        ("dep.wi", dep),
        (
            "main.wi",
            "import dep; async fn main() { dep::G::data.push(2); }",
        ),
    ]);
    let (_, result) = fixture.check();
    assert!(
        result
            .labels
            .iter()
            .any(|(p, s)| p.ends_with("main.wi") && s.contains("data")),
        "{result:?}"
    );
    assert!(
        result
            .labels
            .iter()
            .any(|(p, s)| p.ends_with("dep.wi") && s.contains("data")),
        "{result:?}"
    );
}

#[test]
fn static_task_sharing_cache_rechecks_reachability_and_field_mutability() {
    let immutable = "class C { pub n: i64; } class G { pub static c: C = new C(0); } fn helper() { println(G::c.n); }";
    let fixture = Fixture::new(&[(
        "main.wi",
        &format!("{immutable} async fn main() {{ helper(); }}"),
    )]);
    assert!(fixture.check().0);
    fs::write(
        fixture.0.join("main.wi"),
        format!("{immutable} fn mutate(c: C) {{ c.n = 1; }} async fn main() {{ helper(); }}"),
    )
    .unwrap();
    let (ok, result) = fixture.check();
    assert!(!ok && sharing_errors(&result) > 0, "{result:?}");
    fs::write(
        fixture.0.join("main.wi"),
        format!("{immutable} fn mutate(c: C) {{ c.n = 1; }} async fn main() {{}} "),
    )
    .unwrap();
    assert!(fixture.check().0);
    // The successful revision stored a non-Sync read in an unused helper.
    // Connecting that unchanged body to a task must activate its cached obligation.
    fs::write(
        fixture.0.join("main.wi"),
        format!("{immutable} fn mutate(c: C) {{ c.n = 1; }} async fn main() {{ helper(); }}"),
    )
    .unwrap();
    let (ok, result) = fixture.check();
    assert!(!ok && sharing_errors(&result) > 0, "{result:?}");
    fs::write(
        fixture.0.join("main.wi"),
        format!("{immutable} async fn main() {{ helper(); }}"),
    )
    .unwrap();
    assert!(fixture.check().0);
}

#[test]
fn static_task_sharing_generic_statics_and_imported_dispatch() {
    rejected(&[(
        "main.wi",
        "import std::collections::Array; class Cell<T> { static data: Array<T> = []; pub fn size(self) -> i64 { return Self::data.len(); } } async fn main() { println(new Cell<i64>().size()); }",
    )]);
    rejected(&[
        (
            "dep.wi",
            "module dep; import std::collections::Array; pub class G { pub static data: Array<i64> = [1]; } pub interface Run { fn run(self); } pub fn invoke(r: Run) { r.run(); }",
        ),
        (
            "main.wi",
            "import dep; class R implements dep::Run { pub fn run(self) { dep::G::data.push(2); } } async fn main() { dep::invoke(new R()); }",
        ),
    ]);
    rejected(&[
        (
            "dep.wi",
            "module dep; import std::collections::Array; class G { pub static data: Array<i64> = [1]; } pub interface Run { fn run(self) { G::data.push(2); } }",
        ),
        (
            "main.wi",
            "import dep; class R implements dep::Run {} async fn main() { new R().run(); }",
        ),
    ]);
    rejected(&[
        (
            "dep.wi",
            "module dep; import std::collections::Array; class G { pub static data: Array<i64> = [1]; } pub fn callback() { G::data.push(2); }",
        ),
        (
            "middle.wi",
            "module middle; import dep::callback; pub fn call() { let f = callback; f(); }",
        ),
        (
            "main.wi",
            "import middle; async fn main() { middle::call(); }",
        ),
    ]);
    accepted(
        "interface Sync {} interface Value extends Sync { fn get(self) -> i64; } class Box implements Value { pub fn get(self) -> i64 { return 1; } } class G { pub static value: Value = new Box(); } async fn main() { println(G::value.get()); }",
    );
}

#[test]
fn static_task_sharing_nested_lambda_cache_tracks_marker_dependencies() {
    let prefix = "class C { pub n: i64; } class G { pub static c: C = new C(0); }";
    let body = "fn helper() { println(G::c.n); let f = || { println(G::c.n); }; f(); } async fn main() { helper(); }";
    let fixture = Fixture::new(&[("main.wi", &format!("{prefix} {body}"))]);
    assert!(fixture.check().0);
    fs::write(
        fixture.0.join("main.wi"),
        format!("{prefix} fn mutate(c: C) {{ c.n = 2; }} {body}"),
    )
    .unwrap();
    let (ok, result) = fixture.check();
    assert!(!ok, "{result:?}");
    assert_eq!(sharing_errors(&result), 2, "{result:?}");
}

#[test]
fn static_task_sharing_imported_helper_rechecks_global_field_writes() {
    let dep = "module dep; pub class C { pub n: i64; } pub class G { pub static c: C = new C(0); } pub fn helper() { println(G::c.n); }";
    let entry = "import dep; async fn main() { dep::helper(); }";
    let fixture = Fixture::new(&[("dep.wi", dep), ("main.wi", entry)]);
    assert!(fixture.check().0);
    fs::write(
        fixture.0.join("main.wi"),
        format!("{entry} fn mutate(c: dep::C) {{ c.n = 2; }}"),
    )
    .unwrap();
    let (ok, result) = fixture.check();
    assert!(!ok && sharing_errors(&result) > 0, "{result:?}");
    fs::write(fixture.0.join("main.wi"), entry).unwrap();
    assert!(fixture.check().0);
}
