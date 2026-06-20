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
        },
        async_tasks_case(),
    ]
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
    }
}

fn async_tasks_case() -> BenchmarkCase {
    let mut source = String::from(
        "async fn work(value: i64) -> i64 { await sleep(0); return value; }\nfn main() {\n",
    );
    for index in 0..32 {
        source.push_str(&format!("let task_{index} = work({index});\n"));
    }
    source.push_str("let mut total = 0;\n");
    for index in 0..32 {
        source.push_str(&format!("total = total + task_{index}.join();\n"));
    }
    source.push_str("println(total);\n}\n");
    BenchmarkCase {
        name: "async_tasks",
        files: vec![("main.wi".into(), source)],
        entry: "main.wi",
    }
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("benchmark paths must be valid UTF-8")
}
