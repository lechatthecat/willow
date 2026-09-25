//! Deterministic query counts across distinct module graph shapes.
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-compiler-db-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn write(&self, module: &str, source: impl AsRef<str>) {
        std::fs::write(self.0.join(format!("{module}.wi")), source.as_ref()).unwrap();
    }

    fn build(&self) -> Output {
        self.command()
            .arg("-o")
            .arg(self.0.join("app"))
            .output()
            .unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_willowc"));
        command
            .current_dir(&self.0)
            .arg("build")
            .arg("main.wi")
            .env("WILLOW_QUERY_STATS", "1");
        command
    }

    /// Everything a build reports, with per-query timings and cargo status
    /// lines normalized so two runs are comparable byte for byte:
    /// diagnostics, `[lir]`, `[panic-effects]` and `[query-stats]` lines, the
    /// LIR dump, and the program output.
    fn observable_outputs(&self) -> Vec<String> {
        let build = self
            .command()
            .arg("-o")
            .arg(self.0.join("app"))
            .env("WILLOW_LIR_LOG", "1")
            .env("WILLOW_PANIC_EFFECTS_LOG", "1")
            .output()
            .unwrap();
        let normalize = |bytes: Vec<u8>| {
            let mut text = String::from_utf8(bytes)
                .unwrap()
                .replace(self.0.to_str().unwrap(), "<project>")
                .lines()
                // The runtime auto-build's cargo status line carries a
                // wall-clock duration; it is ANSI-colored when
                // CARGO_TERM_COLOR=always (as in CI).
                .filter(|line| !strip_ansi(line).trim_start().starts_with("Finished `"))
                .fold(String::new(), |mut text, line| {
                    text.push_str(line);
                    text.push('\n');
                    text
                });
            let mut position = 0;
            while let Some(start) = text[position..].find("compute_ns=") {
                let start = position + start + "compute_ns=".len();
                let end = text[start..]
                    .find(|c: char| !c.is_ascii_digit())
                    .map_or(text.len(), |end| start + end);
                text.replace_range(start..end, "_");
                position = start + 1;
            }
            text
        };
        let stderr = normalize(build.stderr);
        let lir = self.command().arg("--emit-lir").output().unwrap();
        let run = if build.status.success() {
            let run = Command::new(self.0.join("app")).output().unwrap();
            format!(
                "{:?}\n{}",
                run.status.code(),
                String::from_utf8_lossy(&run.stdout)
            )
        } else {
            String::new()
        };
        vec![
            format!("{:?}", build.status.code()),
            stderr,
            String::from_utf8(lir.stdout).unwrap(),
            normalize(lir.stderr),
            run,
        ]
    }

    fn assert_build(&self, units: usize, expected: usize) {
        let output = self.build();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        let reports: Vec<_> = stderr
            .lines()
            .filter(|line| line.starts_with("[query-stats] "))
            .collect();
        assert_eq!(reports.len(), 1, "{stderr}");
        for (key, expected) in [
            ("type_checkers", units),
            ("nonpreemptible_helpers", units),
            ("peak_ast", 1),
            ("peak_checker", 1),
            ("peak_declared", 1),
            ("peak_lir", 1),
        ] {
            let value = reports[0]
                .split_whitespace()
                .filter_map(|field| field.split_once('='))
                .find_map(|(name, value)| (name == key).then_some(value))
                .unwrap_or_else(|| panic!("missing {key}: {stderr}"));
            assert_eq!(value, expected.to_string(), "{key}: {stderr}");
        }
        assert_two_hydrates_per_unit(reports[0], units, &stderr);
        for query in [
            "visible_scope",
            "unit_scope",
            "typed_body",
            "normalized_body",
            "lir_body",
            "lir_unit",
        ] {
            let record = reports[0]
                .split_whitespace()
                .find_map(|field| field.strip_prefix(&format!("{query}[")))
                .unwrap_or_else(|| panic!("missing {query}: {stderr}"));
            let computations = record
                .split(',')
                .find_map(|field| field.strip_prefix("computations="))
                .unwrap();
            assert_eq!(computations, units.to_string(), "{query}: {stderr}");
        }
        let run = Command::new(self.0.join("app")).output().unwrap();
        assert!(run.status.success(), "{run:?}");
        assert_eq!(
            String::from_utf8_lossy(&run.stdout),
            format!("{expected}\n")
        );
    }
}

/// Remove ANSI CSI escape sequences (`ESC [ ... final-byte`).
fn strip_ansi(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            plain.push(c);
        }
    }
    plain
}

/// Each unit is hydrated once to check it and once to declare it, in debug
/// builds too: debug metadata reads the declare-pass tree (willow-afb5.13).
fn assert_two_hydrates_per_unit(report: &str, units: usize, stderr: &str) {
    let by_file = report
        .split_whitespace()
        .find_map(|field| field.strip_prefix("hydrates_by_file="))
        .unwrap_or_else(|| panic!("missing hydrates_by_file: {stderr}"));
    let counts: Vec<_> = by_file
        .split(',')
        .map(|pair| pair.split_once(':').unwrap().1)
        .collect();
    assert_eq!(counts.len(), units, "hydrates_by_file: {stderr}");
    assert!(
        counts.iter().all(|count| *count == "2"),
        "hydrates_by_file: {stderr}"
    );
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn debug_and_release_builds_hydrate_each_unit_twice() {
    let project = Project::new();
    project.write("leaf", "pub fn value() -> i64 { return 1; }");
    project.write(
        "mid",
        "import leaf; pub fn value() -> i64 { return leaf::value() + 1; }",
    );
    project.write(
        "main",
        "import leaf; import mid; fn main() { println(mid::value() + leaf::value()); }",
    );
    for release in [false, true] {
        let mut command = project.command();
        if release {
            command.arg("--release");
        }
        let output = command
            .arg("-o")
            .arg(project.0.join("app"))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        let report = stderr
            .lines()
            .find(|line| line.starts_with("[query-stats] "))
            .unwrap_or_else(|| panic!("no report: {stderr}"));
        assert_two_hydrates_per_unit(report, 3, &stderr);
        let source_map = project.0.join("app.wsmap");
        assert_eq!(source_map.exists(), !release, "{stderr}");
        if !release {
            let text = std::fs::read_to_string(source_map).unwrap();
            assert_eq!(text.matches("\n---\n").count(), 2, "{text}");
        }
        let run = Command::new(project.0.join("app")).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&run.stdout), "3\n");
    }
}

#[test]
fn chain_queries_compute_once_per_unit() {
    for count in [1, 4, 16] {
        let project = Project::new();
        project.write("unit0", "pub fn value() -> i64 { return 1; }");
        for index in 1..count {
            let previous = index - 1;
            project.write(
                &format!("unit{index}"),
                format!("import unit{previous}; pub fn value() -> i64 {{ return unit{previous}::value() + 1; }}"),
            );
        }
        let last = count - 1;
        project.write(
            "main",
            format!("import unit{last}; fn main() {{ println(unit{last}::value()); }}"),
        );
        project.assert_build(count + 1, count);
    }
}

#[test]
fn fanout_queries_compute_once_per_unit() {
    for count in [1, 4, 16] {
        let project = Project::new();
        let mut imports = String::new();
        let mut calls = Vec::new();
        for index in 0..count {
            project.write(
                &format!("leaf{index}"),
                format!("pub fn value() -> i64 {{ return {}; }}", index + 1),
            );
            imports.push_str(&format!("import leaf{index};\n"));
            calls.push(format!("leaf{index}::value()"));
        }
        project.write(
            "main",
            format!("{imports} fn main() {{ println({}); }}", calls.join(" + ")),
        );
        project.assert_build(count + 1, count * (count + 1) / 2);
    }
}

#[test]
fn diamonds_aliases_and_repeated_imports_share_queries() {
    for count in [1, 4, 16] {
        let project = Project::new();
        project.write("shared", "pub fn value() -> i64 { return 1; }");
        let mut imports = String::from(
            "import shared as direct; import shared as direct; import shared::value as shared_value;\n",
        );
        let mut calls = vec!["direct::value()".to_string(), "shared_value()".to_string()];
        for index in 0..count {
            project.write(
                &format!("branch{index}"),
                "import shared as base; import shared::value as alias; pub fn value() -> i64 { return base::value() + alias(); }",
            );
            imports.push_str(&format!("import branch{index} as arm{index};\n"));
            calls.push(format!("arm{index}::value()"));
        }
        project.write(
            "main",
            format!("{imports} fn main() {{ println({}); }}", calls.join(" + ")),
        );
        project.assert_build(count + 2, 2 * count + 2);
    }
}

#[test]
fn importing_file_back_to_entry_is_a_cycle() {
    let project = Project::new();
    project.write("main", "import worker; fn main() { worker::run(); }");
    project.write("worker", "import main; pub fn run() {}");
    let output = project.build();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "entry cycle unexpectedly compiled"
    );
    assert!(stderr.contains("E0403"), "{stderr}");
    assert!(!project.0.join("app").exists(), "cycle emitted a binary");
}

#[test]
fn injected_default_closures_keep_concrete_body_identity() {
    let project = Project::new();
    project.write(
        "main",
        r#"
        interface Value {
            fn value(self) -> i64 {
                let extra = 40;
                let f = |x: i64| { let nested = |y: i64| y + extra; return nested(x); };
                return f(2);
            }
        }
        class A implements Value {}
        class B implements Value {}
        fn main() { let a = new A(); let b = new B(); println(a.value() + b.value()); }
    "#,
    );
    let output = project.build();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new(project.0.join("app")).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "84\n");
}

#[test]
fn imported_default_closures_are_bound_to_each_concrete_unit() {
    let project = Project::new();
    project.write(
        "proto",
        r#"
        pub interface Value {
            fn value(self) -> i64 {
                let extra = 40;
                let f = |x: i64| { let nested = |y: i64| y + extra; return nested(x); };
                return f(2);
            }
        }
    "#,
    );
    project.write("a", "import proto::Value; pub class A implements Value {}");
    project.write("b", "import proto::Value; pub class B implements Value {}");
    project.write("main", "import a; import b; fn main() { let x = new a::A(); let y = new b::B(); println(x.value() + y.value()); }");
    let output = project.build();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new(project.0.join("app")).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "84\n");
}

/// A project whose main imports the same units in the given order. Every
/// query kind is exercised: cross-unit inheritance and slots, an injected
/// interface default, lambdas with resolved panic-effect edges, and errors
/// in three different units.
fn write_permutable_project(project: &Project, imports: &[&str], with_errors: bool) {
    project.write(
        "base",
        r#"
        pub open class Base {
            pub fn base_value(self) -> i64 { return 1; }
            pub open fn name(self) -> String { return "base"; }
        }
        pub interface Describe { fn describe(self) -> String { let f = |x: String| { return x + "!"; }; return f("d"); } }
        pub fn risky(x: i64) -> i64 { if x < 0 { panic("negative"); } return x; }
        "#,
    );
    project.write(
        "leaf",
        r#"
        import base;
        pub class Leaf extends base::Base implements base::Describe {
            pub override fn name(self) -> String { return "leaf"; }
        }
        pub fn value() -> i64 { let g = |x: i64| { return base::risky(x) * 2; }; let h = |x: i64| { return x; }; return g(3) + h(0); }
        "#,
    );
    project.write(
        "other",
        r#"
        import base;
        pub class Other extends base::Base implements base::Describe {}
        pub fn value() -> i64 { return base::risky(4); }
        "#,
    );
    let errors = if with_errors {
        "let broken: i64 = \"text\";"
    } else {
        ""
    };
    let imports: Vec<_> = imports
        .iter()
        .map(|unit| format!("import {unit};"))
        .collect();
    project.write(
        "main",
        format!(
            "{}\nfn main() {{ {errors} let l = new leaf::Leaf(); let o = new other::Other(); \
             println(l.name() + o.name() + l.describe() + o.describe()); \
             println(leaf::value() + other::value() + l.base_value()); }}",
            imports.join(" ")
        ),
    );
    if with_errors {
        project.write(
            "other",
            "import base; pub class Other extends base::Base {} pub fn value() -> bool { return base::risky(4); }",
        );
        project.write(
            "leaf",
            "import base; pub fn value() -> i64 { return base::risky(\"bad\"); }\n\
             pub class Leaf extends base::Base { override fn name(self) -> String { return 1; } }",
        );
    }
}

/// Split a report into records (one diagnostic with its labels, one log
/// line, one stats line) so the set of records can be compared when only the
/// order in which independent units were reached differs.
fn records(report: &str) -> Vec<String> {
    let mut records: Vec<String> = Vec::new();
    for line in report.lines() {
        let starts_record = [
            "error[",
            "warning[",
            "[lir]",
            "[panic-effects]",
            "[query-stats]",
            "compiled ",
            "Error:",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix));
        match records.last_mut() {
            Some(current) if !starts_record => {
                current.push('\n');
                current.push_str(line);
            }
            _ => records.push(line.to_string()),
        }
    }
    records.sort();
    records
}

/// Spec 9.4: results are a function of the inputs, never of the order in
/// which the driver evaluated queries. Repeated runs of one project permute
/// hash-seeded iteration and must be byte-identical. Permuting the import
/// order permutes unit/file ids and the order independent units are reached;
/// every per-unit record, the entry LIR dump and the program output must
/// still be identical.
#[test]
fn permuted_evaluation_order_gives_identical_diagnostics_and_lir() {
    for with_errors in [false, true] {
        let mut outputs = Vec::new();
        for imports in [
            ["base", "leaf", "other"],
            ["other", "leaf", "base"],
            ["leaf", "other", "base"],
        ] {
            let project = Project::new();
            write_permutable_project(&project, &imports, with_errors);
            let first = project.observable_outputs();
            let second = project.observable_outputs();
            assert!(
                first == second,
                "repeated build differs:\n{first:#?}\n---\n{second:#?}"
            );
            outputs.push(first);
        }
        let expected = &outputs[0];
        assert_eq!(
            expected[0],
            if with_errors { "Some(1)" } else { "Some(0)" },
            "{}",
            expected[1]
        );
        if with_errors {
            assert!(expected[1].contains("E0201"), "{}", expected[1]);
            assert!(expected[3].contains("E0201"), "{}", expected[3]);
        } else {
            assert!(expected[1].contains("[lir] compiling"), "{}", expected[1]);
            // $lambda.0 is the copied default's lambda whose interface lives
            // in another unit (conservative), 1 calls `risky`, 2 is call-free.
            for (index, fact) in [(0, "may-panic"), (1, "may-panic"), (2, "no-panic")] {
                let line = format!("[panic-effects] leaf.$lambda.{index}: {fact}");
                assert!(expected[1].contains(&line), "{line}\n{}", expected[1]);
            }
            assert!(expected[2].contains("fn main"), "{}", expected[2]);
            assert_eq!(expected[4], "Some(0)\nleafbased!d!\n11\n");
        }
        for (index, actual) in outputs.iter().enumerate().skip(1) {
            for (field, (expected, actual)) in expected.iter().zip(actual).enumerate() {
                let same = match field {
                    1 | 3 => records(expected) == records(actual),
                    _ => expected == actual,
                };
                assert!(
                    same,
                    "permutation {index} field {field} differs:\n{expected}\n---\n{actual}"
                );
            }
        }
    }
}

/// Queries return diagnostics as values; only the driver may print or emit
/// them. Test-only code under `#[cfg(test)]` is exempt.
const FORBIDDEN_IN_QUERIES: &[&str] = &[
    "eprintln!",
    "println!",
    "eprint!",
    "print!",
    "emit_all",
    "emit_with",
    "emit_frontend_diagnostics",
];

/// Removes `//` comments and every item or statement guarded by
/// `#[cfg(test)]`, so the remaining text is the non-test code.
fn non_test_code(source: &str) -> String {
    let uncommented: String = source
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let bytes = uncommented.as_bytes();
    let mut kept = String::with_capacity(uncommented.len());
    let mut cursor = 0;
    while let Some(offset) = uncommented[cursor..].find("#[cfg(test)]") {
        let start = cursor + offset;
        kept.push_str(&uncommented[cursor..start]);
        let mut index = start + "#[cfg(test)]".len();
        let mut depth = 0usize;
        while index < bytes.len() {
            match bytes[index] {
                b'{' | b'(' | b'[' => depth += 1,
                b'}' | b')' | b']' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 && bytes[index] == b'}' {
                        index += 1;
                        break;
                    }
                }
                b'}' | b')' | b']' => break,
                b';' | b',' if depth == 0 => {
                    index += 1;
                    break;
                }
                _ => {}
            }
            index += 1;
        }
        cursor = index;
    }
    kept.push_str(&uncommented[cursor..]);
    kept
}

fn rust_files(dir: &std::path::Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn compiler_db_queries_never_emit_diagnostics() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/compiler_db");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(!files.is_empty(), "no sources under {}", root.display());
    let mut violations = Vec::new();
    for file in files {
        let code = non_test_code(&std::fs::read_to_string(&file).unwrap());
        for needle in FORBIDDEN_IN_QUERIES {
            if code.contains(needle) {
                violations.push(format!("{}: {needle}", file.display()));
            }
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn diagnostic_scanner_strips_only_test_code() {
    let source = "fn keep() { a(); }\n#[cfg(test)]\nmod tests { fn t() { eprintln!(\"x\"); } }\n\
                  struct S {\n    #[cfg(test)]\n    work: Cell<[usize; 4]>,\n    live: u8,\n}\n\
                  fn f() {\n    #[cfg(test)]\n    self.count(0);\n    println!(\"live\");\n}\n\
                  // eprintln! in a comment\n";
    let code = non_test_code(source);
    assert!(!code.contains("eprintln!"), "{code}");
    assert!(!code.contains("work"), "{code}");
    assert!(!code.contains("count"), "{code}");
    assert!(code.contains("live: u8"), "{code}");
    assert!(code.contains("println!(\"live\")"), "{code}");
    assert!(code.contains("fn keep"), "{code}");
}
