use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use willow_compiler::{CompilerOptions, compile};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct BenchmarkCase {
    name: &'static str,
    files: Vec<(String, String)>,
    entry: &'static str,
    /// Compiler-only panic-effect switch. Runtime execution does not consult
    /// it; this lets one benchmark invocation record optimized and legacy
    /// codegen side by side (willow-s9ej.8).
    panic_effects: Option<&'static str>,
}

struct TempBenchmark {
    root: PathBuf,
    binary: PathBuf,
}

impl TempBenchmark {
    fn new(case: &BenchmarkCase) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "willow_bench_{}_{}_{}",
            case.name,
            std::process::id(),
            id
        ));
        fs::create_dir_all(&root).unwrap();
        for (path, source) in &case.files {
            let path = root.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, source).unwrap();
        }
        let binary = root.join("program");
        Self { root, binary }
    }
}

impl Drop for TempBenchmark {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn main() {
    let iterations = std::env::var("WILLOW_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|iterations| *iterations > 0)
        .unwrap_or(3);

    println!("case,iteration,compile_ms,artifact_bytes,run_ms");
    for case in benchmark_cases() {
        for iteration in 1..=iterations {
            run_case(&case, iteration);
        }
    }
}

fn run_case(case: &BenchmarkCase, iteration: usize) {
    let project = TempBenchmark::new(case);
    let entry = project.root.join(case.entry);
    let started = Instant::now();
    let _panic_effects = CompilerEnvOverride::new("WILLOW_PANIC_EFFECTS", case.panic_effects);
    compile(
        path_str(&entry),
        path_str(&project.binary),
        &CompilerOptions::release(),
        Some(project.root.clone()),
    )
    .unwrap_or_else(|error| panic!("{} compile failed: {error:#}", case.name));
    let compile_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let artifact_bytes = fs::metadata(&project.binary).unwrap().len();

    let started = Instant::now();
    let output = Command::new(&project.binary).output().unwrap();
    let run_ms = started.elapsed().as_secs_f64() * 1_000.0;
    assert!(output.status.success(), "{} execution failed", case.name);

    println!(
        "{},{iteration},{compile_ms:.3},{artifact_bytes},{run_ms:.3}",
        case.name
    );
}

fn benchmark_cases() -> Vec<BenchmarkCase> {
    vec![
        BenchmarkCase {
            name: "fib",
            files: vec![(
                "main.wi".into(),
                "fn fib(n: i64) -> i64 { if n <= 1 { return n; } return fib(n - 1) + fib(n - 2); } fn main() { println(fib(30)); }".into(),
            )],
            entry: "main.wi",
            panic_effects: None,
        },
        huge_source_case(),
        many_modules_case(),
        BenchmarkCase {
            name: "gc_pause_workload",
            files: vec![(
                "main.wi".into(),
                "class Node { pub value: i64; } fn main() { let mut i = 0; while i < 5000 { let node = new Node(i); if i % 50 == 0 { gc_collect(); } i = i + 1; } gc_collect(); println(i); }".into(),
            )],
            entry: "main.wi",
            panic_effects: None,
        },
        option_linked_list_case(),
        option_sugar_linked_list_case(),
        async_tasks_case(),
        BenchmarkCase {
            name: "pow_f64_literal_chain",
            files: vec![(
                "main.wi".into(),
                "fn main() { let mut i = 0; let mut total = 0.0; while i < 100000 { total = total + 1.000001 ** 2.0 + 1.000001 ** 8.0; i = i + 1; } println(total); }".into(),
            )],
            entry: "main.wi",
            panic_effects: None,
        },
        BenchmarkCase {
            name: "pow_i64_dynamic",
            files: vec![(
                "main.wi".into(),
                "fn p(x: i64, y: i64) -> i64 { return x ** y; } fn main() { let mut i = 0; let mut total = 0; while i < 100000 { total = total + p(3, i % 32); i = i + 1; } println(total); }".into(),
            )],
            entry: "main.wi",
            panic_effects: None,
        },
        BenchmarkCase {
            name: "pow_f64_integral",
            files: vec![(
                "main.wi".into(),
                "fn p(x: f64, y: f64) -> f64 { return x ** y; } fn main() { let mut i = 0; let mut exponent = 1.0; let mut total = 0.0; while i < 100000 { total = total + p(1.000001, exponent); exponent = exponent + 1.0; if exponent > 16.0 { exponent = 1.0; } i = i + 1; } println(total); }".into(),
            )],
            entry: "main.wi",
            panic_effects: None,
        },
        BenchmarkCase {
            name: "pow_f64_generic",
            files: vec![(
                "main.wi".into(),
                "fn p(x: f64, y: f64) -> f64 { return x ** y; } fn main() { let mut i = 0; let mut exponent = 0.500001; let mut total = 0.0; while i < 100000 { total = total + p(1.000001, exponent); exponent = exponent + 0.000001; i = i + 1; } println(total); }".into(),
            )],
            entry: "main.wi",
            panic_effects: None,
        },
        class_dispatch_case("class_dispatch_chain_1", 1),
        class_dispatch_case("class_dispatch_chain_8", 8),
        class_dispatch_case("class_dispatch_chain_32", 32),
        class_dispatch_call_sites_case("class_dispatch_call_sites_1", 1),
        class_dispatch_call_sites_case("class_dispatch_call_sites_32", 32),
        class_dispatch_hierarchy_case("class_dispatch_hierarchy_32_first", 32, 0),
        class_dispatch_hierarchy_case("class_dispatch_hierarchy_32_last", 32, 31),
        class_dispatch_baseline_case("class_dispatch_baseline_freefn", false),
        class_dispatch_baseline_case("class_dispatch_baseline_fieldread", true),
        panic_effect_call_chain_case("panic_effect_calls_optimized", Some("1")),
        panic_effect_call_chain_case("panic_effect_calls_baseline", Some("0")),
        panic_effect_defer_case("panic_effect_defer_optimized", Some("1")),
        panic_effect_defer_case("panic_effect_defer_baseline", Some("0")),
        panic_recover_request_case("panic_recover_requests_0pct", 0),
        panic_recover_request_case("panic_recover_requests_1pct", 100),
        panic_recover_request_case("panic_recover_requests_10pct", 10),
    ]
}

/// Paired with `option_sugar_gc_ref_linked_list`: both allocate exactly one
/// same-sized node per iteration. The explicit spelling must not add wrapper
/// allocations, and its run time should stay level with the permanent `T?`
/// parser sugar. The removed pre-migration `nil`/implicit-lift form is no
/// longer valid Willow and therefore cannot serve as an executable baseline.
fn option_linked_list_case() -> BenchmarkCase {
    BenchmarkCase {
        name: "option_gc_ref_linked_list",
        files: vec![(
            "main.wi".into(),
            "class Node { pub value: i64; pub next: Option<Node>; }\n\
             fn main() { let mut head: Option<Node> = None; let mut i = 0; while i < 100000 { head = Some(new Node(i, head)); i = i + 1; } println(i); }"
                .into(),
        )],
        entry: "main.wi",
        panic_effects: None,
    }
}

fn option_sugar_linked_list_case() -> BenchmarkCase {
    BenchmarkCase {
        name: "option_sugar_gc_ref_linked_list",
        files: vec![(
            "main.wi".into(),
            "class Node { pub value: i64; pub next: Node?; }\n\
             fn main() { let mut head: Node? = None; let mut i = 0; while i < 100000 { head = Some(new Node(i, head)); i = i + 1; } println(i); }"
                .into(),
        )],
        entry: "main.wi",
        panic_effects: None,
    }
}

fn huge_source_case() -> BenchmarkCase {
    let mut source = String::new();
    for index in 0..1_000 {
        source.push_str(&format!(
            "fn function_{index}(value: i64) -> i64 {{ return value + {index}; }}\n"
        ));
    }
    source.push_str("fn main() { println(function_999(1)); }\n");
    BenchmarkCase {
        name: "huge_source",
        files: vec![("main.wi".into(), source)],
        entry: "main.wi",
        panic_effects: None,
    }
}

fn many_modules_case() -> BenchmarkCase {
    let mut files = Vec::new();
    let mut entry = String::new();
    for index in 0..25 {
        let module = format!("module_{index}");
        entry.push_str(&format!("import {module};\n"));
        files.push((
            format!("{module}.wi"),
            format!("module {module}; pub fn value() -> i64 {{ return {index}; }}"),
        ));
    }
    entry.push_str("fn main() { println(module_24::value()); }\n");
    files.push(("main.wi".into(), entry));
    BenchmarkCase {
        name: "many_modules",
        files,
        entry: "main.wi",
        panic_effects: None,
    }
}

fn async_tasks_case() -> BenchmarkCase {
    // Waiting on a task is `await`, and `await` needs an async fn, so the
    // generated main must be `async` (willow-qrj9).
    let mut source = String::from(
        "async fn work(value: i64) -> i64 { await sleep(0); return value; }\nasync fn main() {\n",
    );
    for index in 0..32 {
        source.push_str(&format!("let task_{index} = work({index});\n"));
    }
    source.push_str("let mut total = 0;\n");
    for index in 0..32 {
        source.push_str(&format!("total = total + await task_{index};\n"));
    }
    source.push_str("println(total);\n}\n");
    BenchmarkCase {
        name: "async_tasks",
        files: vec![("main.wi".into(), source)],
        entry: "main.wi",
        panic_effects: None,
    }
}

/// Class-method dispatch cost as a function of how many classes define the
/// method (willow-uqzx, catalog item 6).
///
/// A call on a class-typed receiver lowers to an inline chain: load the
/// object's runtime `type_id` from word 0, then compare it against every class
/// that resolves a method of that name, each arm carrying its own copy of the
/// argument-evaluation and call sequence. The arms are NOT restricted to the
/// receiver's own hierarchy — the dispatch list is built from every class in
/// the program that has a matching method name — so an unrelated class with the
/// same method name lengthens the chain at every call site.
///
/// These cases hold the number of calls constant and vary only the number of
/// classes defining `step`, so `run_ms` isolates dispatch latency and
/// `artifact_bytes` isolates the code the chain costs.
fn class_dispatch_case(name: &'static str, classes: usize) -> BenchmarkCase {
    let mut source = String::new();
    for index in 0..classes {
        source.push_str(&format!(
            "class C{index} {{ pub v: i64; pub fn step(self) -> i64 {{ return self.v + {index}; }} }}\n"
        ));
    }
    source.push_str("fn main() {\n");
    // Keep every class live so release-mode dead-code removal cannot shrink the
    // dispatch chain out from under the measurement.
    source.push_str("let mut warm = 0;\n");
    for index in 0..classes {
        source.push_str(&format!("warm = warm + new C{index}({index}).step();\n"));
    }
    source.push_str(
        "let target = new C0(1);\n\
         let mut i = 0;\n\
         let mut total = 0;\n\
         while i < 1000000 { total = total + target.step(); i = i + 1; }\n\
         println(total + warm);\n}\n",
    );
    BenchmarkCase {
        name,
        files: vec![("main.wi".into(), source)],
        entry: "main.wi",
        panic_effects: None,
    }
}

/// The code-size half of the same question: the dispatch chain is emitted
/// inline at every call site, so cost scales with classes x call sites rather
/// than with classes alone. These cases hold the call sites constant (256) and
/// vary the class count, so the `artifact_bytes` delta is the chain duplication.
fn class_dispatch_call_sites_case(name: &'static str, classes: usize) -> BenchmarkCase {
    let mut source = String::new();
    for index in 0..classes {
        source.push_str(&format!(
            "class C{index} {{ pub v: i64; pub fn step(self) -> i64 {{ return self.v + {index}; }} }}\n"
        ));
    }
    for site in 0..256 {
        source.push_str(&format!(
            "fn site_{site}(c: C0) -> i64 {{ return c.step() + {site}; }}\n"
        ));
    }
    source.push_str("fn main() {\nlet mut warm = 0;\n");
    for index in 0..classes {
        source.push_str(&format!("warm = warm + new C{index}({index}).step();\n"));
    }
    source.push_str("let target = new C0(1);\nlet mut total = 0;\n");
    for site in 0..256 {
        source.push_str(&format!("total = total + site_{site}(target);\n"));
    }
    source.push_str("println(total + warm);\n}\n");
    BenchmarkCase {
        name,
        files: vec![("main.wi".into(), source)],
        entry: "main.wi",
        panic_effects: None,
    }
}

/// Calibration for the dispatch cases: the same 1,000,000-iteration loop with
/// the method call replaced by a free-function call, or by a bare field read.
/// Without these there is no way to tell "dispatch is cheap" from "the loop
/// overhead is large enough to hide it".
fn class_dispatch_baseline_case(name: &'static str, field_read_only: bool) -> BenchmarkCase {
    let body = if field_read_only {
        "total = total + target.v;"
    } else {
        "total = total + step_free(target.v);"
    };
    let source = format!(
        "class C0 {{ pub v: i64; }}\n\
         fn step_free(v: i64) -> i64 {{ return v; }}\n\
         fn main() {{\n\
         let target = new C0(1);\n\
         let mut i = 0;\n\
         let mut total = 0;\n\
         while i < 1000000 {{ {body} i = i + 1; }}\n\
         println(total);\n}}\n"
    );
    BenchmarkCase {
        name,
        files: vec![("main.wi".into(), source)],
        entry: "main.wi",
        panic_effects: None,
    }
}

/// The genuinely polymorphic case: one open base and `subclasses` overrides,
/// with the receiver statically typed as the base so the call really is
/// dynamic. The chain's arms are ordered by runtime `type_id`, so the cost of a
/// call depends on WHERE the receiver's class sits in that order, not on how
/// long the chain is. `hot_index` selects the class the hot loop actually calls:
/// index 0 hits the first arm, index `subclasses - 1` walks the whole chain.
/// The gap between the two is the dispatch penalty a vtable would remove.
fn class_dispatch_hierarchy_case(
    name: &'static str,
    subclasses: usize,
    hot_index: usize,
) -> BenchmarkCase {
    let mut source = String::from(
        "open class Base { pub v: i64; pub open fn step(self) -> i64 { return self.v; } }\n",
    );
    for index in 0..subclasses {
        source.push_str(&format!(
            "class D{index} extends Base {{ pub override fn step(self) -> i64 {{ return self.v + {index}; }} }}\n"
        ));
    }
    source.push_str("fn main() {\nlet mut warm = 0;\n");
    for index in 0..subclasses {
        source.push_str(&format!("let d{index}: Base = new D{index}({index});\n"));
        source.push_str(&format!("warm = warm + d{index}.step();\n"));
    }
    source.push_str(&format!(
        "let target: Base = d{hot_index};\n\
         let mut i = 0;\n\
         let mut total = 0;\n\
         while i < 1000000 {{ total = total + target.step(); i = i + 1; }}\n\
         println(total + warm);\n}}\n"
    ));
    BenchmarkCase {
        name,
        files: vec![("main.wi".into(), source)],
        entry: "main.wi",
        panic_effects: None,
    }
}

fn panic_effect_call_chain_case(
    name: &'static str,
    panic_effects: Option<&'static str>,
) -> BenchmarkCase {
    BenchmarkCase {
        name,
        files: vec![(
            "main.wi".into(),
            "fn a(n: i64) -> i64 { return n + 1; }\n\
             fn b(n: i64) -> i64 { return a(n) + 1; }\n\
             fn c(n: i64) -> i64 { return b(n) + 1; }\n\
             fn d(n: i64) -> i64 { return c(n) + 1; }\n\
             fn main() { let mut i = 0; let mut total = 0; while i < 1000000 { total = total + d(i); i = i + 1; } println(total); }"
                .into(),
        )],
        entry: "main.wi",
        panic_effects,
    }
}

fn panic_effect_defer_case(
    name: &'static str,
    panic_effects: Option<&'static str>,
) -> BenchmarkCase {
    BenchmarkCase {
        name,
        files: vec![(
            "main.wi".into(),
            "fn cleanup() {}\n\
             fn step(n: i64) -> i64 { defer cleanup(); return n + 1; }\n\
             fn main() { let mut i = 0; let mut total = 0; while i < 250000 { total = total + step(i); i = i + 1; } println(total); }"
                .into(),
        )],
        entry: "main.wi",
        panic_effects,
    }
}

fn panic_recover_request_case(name: &'static str, panic_every: i64) -> BenchmarkCase {
    let failure = if panic_every == 0 {
        "false".to_string()
    } else {
        format!("i % {panic_every} == 0")
    };
    let source = format!(
        "fn request(fail: bool) {{\n\
             if true {{\n\
                 defer match recover() {{ Some(_) => {{}}, None => {{}} }}\n\
                 if fail {{ panic(\"request failed\"); }}\n\
             }}\n\
         }}\n\
         fn main() {{ let mut i = 0; while i < 10000 {{ request({failure}); i = i + 1; }} println(i); }}"
    );
    BenchmarkCase {
        name,
        files: vec![("main.wi".into(), source)],
        entry: "main.wi",
        panic_effects: Some("1"),
    }
}

struct CompilerEnvOverride {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl CompilerEnvOverride {
    fn new(key: &'static str, value: Option<&str>) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: compiler_bench is a single-threaded driver. It changes the
        // variable before entering `compile`, never mutates it concurrently,
        // and restores it before starting the next case.
        unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        Self { key, previous }
    }
}

impl Drop for CompilerEnvOverride {
    fn drop(&mut self) {
        // SAFETY: same single-threaded benchmark invariant as `new`.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("benchmark paths must be valid UTF-8")
}
