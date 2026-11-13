//! End-to-end contract checks through the compiler driver, including cache reuse.
use std::{fs, path::PathBuf, process::Command};

struct Fixture(PathBuf);
impl Fixture {
    fn new(source: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-effect-contracts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("main.wi"), source).unwrap();
        Self(root)
    }
    fn command(&self, command: &str) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.0)
            .args([command, "main.wi"])
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn purity_driver_try_propagation() {
    for (declarations, params, args, result, body, main, output_line) in [
        (
            "",
            "x:Option<i64>",
            "x",
            "Option<i64>",
            "let n = x?; return x;",
            "f(None);",
            None,
        ),
        (
            "class Dest {} class Source implements Into<Dest> { pub dest:Dest; pub fn into(self)->Dest { println(97); return self.dest; } }",
            "x:Result<i64,Source>, y:Result<i64,Dest>",
            "x,y",
            "Result<i64,Dest>",
            "let n = x?; return y;",
            "f(Err(new Source(new Dest())), Ok(7));",
            Some("97"),
        ),
    ] {
        for transitive in [false, true] {
            let functions = if transitive {
                format!(
                    "@pure fn f({params})->{result} {{ return helper({args}); }} fn helper({params})->{result} {{ {body} }}"
                )
            } else {
                format!("@pure fn f({params})->{result} {{ {body} }}")
            };
            let source = format!("{declarations} {functions} fn main() {{ {main} }}");
            let fixture = Fixture::new(&source);
            for command in ["check", "build", "check"] {
                let output = fixture.command(command);
                let text = String::from_utf8_lossy(&output.stderr);
                assert!(!output.status.success(), "{command}: {source}: {text}");
                assert!(text.contains("E2620") && text.contains("UNKNOWN"), "{text}");
                if transitive {
                    assert!(text.contains("f -> helper"), "{text}");
                }
            }
            fs::write(fixture.0.join("main.wi"), source.replace("@pure ", "")).unwrap();
            for command in ["check", "run"] {
                let output = fixture.command(command);
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                if command == "run"
                    && let Some(line) = output_line
                {
                    assert!(
                        String::from_utf8_lossy(&output.stdout)
                            .lines()
                            .any(|s| s == line),
                        "{:?}",
                        output
                    );
                }
            }
        }
    }
}

#[test]
fn purity_driver_string_and_enum_allocations() {
    for (declarations, result, body) in [
        ("", "String", "return a + b;"),
        (
            "enum Pair { Values(i64,i64) }",
            "Pair",
            "return Pair::Values(1,2);",
        ),
        (
            "enum Pair { Values(i64,i64) }",
            "Pair",
            "return Values(1,2);",
        ),
    ] {
        for transitive in [false, true] {
            let functions = if transitive {
                format!(
                    "@pure fn f(a:String,b:String)->{result} {{ return helper(a,b); }} fn helper(a:String,b:String)->{result} {{ {body} }}"
                )
            } else {
                format!("@pure fn f(a:String,b:String)->{result} {{ {body} }}")
            };
            let source = format!("{declarations} {functions} fn main() {{}}");
            let fixture = Fixture::new(&source);
            for command in ["check", "build", "check"] {
                let output = fixture.command(command);
                let text = String::from_utf8_lossy(&output.stderr);
                assert!(!output.status.success(), "{command}: {source}: {text}");
                assert!(text.contains("E2620") && text.contains("UNKNOWN"), "{text}");
                if transitive {
                    assert!(text.contains("f -> helper"), "{text}");
                }
            }
            fs::write(fixture.0.join("main.wi"), source.replace("@pure ", "")).unwrap();
            for command in ["check", "build"] {
                let output = fixture.command(command);
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
}

#[test]
fn purity_driver_closure_allocation() {
    for (body, accepted) in [
        ("let g = || x;", false),
        ("let g: closure() -> i64 = || 1;", false),
        ("let g = || 1;", true),
        ("let g: fn() -> i64 = || { println(1); return 1; };", true),
    ] {
        for transitive in [false, true] {
            let functions = if transitive {
                format!("@pure fn f(x:i64) {{ helper(x); }} fn helper(x:i64) {{ {body} }}")
            } else {
                format!("@pure fn f(x:i64) {{ {body} }}")
            };
            let fixture = Fixture::new(&format!("{functions} fn main() {{}}"));
            for command in ["check", "build", "check"] {
                let output = fixture.command(command);
                let text = String::from_utf8_lossy(&output.stderr);
                assert_eq!(
                    output.status.success(),
                    accepted,
                    "{command}: {functions}: {text}"
                );
                if !accepted {
                    assert!(text.contains("E2620") && text.contains("UNKNOWN"), "{text}");
                    if transitive {
                        assert!(text.contains("f -> helper"), "{text}");
                    }
                }
            }
        }
    }
}

#[test]
fn purity_driver_array_allocation_rejected() {
    for functions in [
        "@pure fn f() { let xs = [1, 2]; }",
        "@pure fn f() { helper(); } fn helper() { let xs = [1, 2]; }",
    ] {
        let fixture = Fixture::new(&format!(
            "import std::collections::Array; {functions} fn main() {{}}"
        ));
        for command in ["check", "build", "check"] {
            let output = fixture.command(command);
            let text = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{command}: {text}");
            assert!(
                text.contains("E2620") && text.contains("UNKNOWN"),
                "{command}: {text}"
            );
            if functions.contains("helper") {
                assert!(text.contains("f -> helper"), "{command}: {text}");
            }
        }
    }
}

#[test]
fn purity_driver_module_revisions() {
    let fixture =
        Fixture::new("import worker; @pure fn f()->i64 { return worker::value(); } fn main() {}");
    for (body, accepted) in [
        ("@pure pub fn value()->i64 { return 7; }", true),
        ("pub fn value()->i64 { println(7); return 7; }", false),
        ("pub fn value()->i64 { return 7; }", true),
    ] {
        fs::write(fixture.0.join("worker.wi"), body).unwrap();
        for _ in 0..2 {
            let output = fixture.command("check");
            let text = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.success(), accepted, "{text}");
            if !accepted {
                assert!(text.contains("E2620"), "{text}");
            }
        }
    }
}

#[test]
fn purity_driver_templates_and_foreign() {
    for (source, accepted, code) in [
        (
            "import std::time; @pure fn f()->i64 { return time::now_ns(); } fn main() {}",
            false,
            "E2620",
        ),
        (
            "@pure fn f()->i64 { return identity(3); } @pure fn identity<T>(x:T)->T { return x; } fn main() {}",
            true,
            "",
        ),
        (
            "@pure fn identity<T>(x:T)->T { return x; } fn main() {}",
            true,
            "",
        ),
        (
            "@pure fn identity<T>(x:T)->T { return x; } fn main() { println(identity(3)); }",
            true,
            "",
        ),
        (
            "@pure fn bad<T>(x:T) { println(1); } fn main() {}",
            false,
            "E2620",
        ),
        (
            "extern rust { fn foreign(); } @pure fn f() { foreign(); } fn main() {}",
            false,
            "E2620",
        ),
        (
            "extern rust { @pure fn foreign(); } fn main() {}",
            false,
            "E0105",
        ),
        (
            "interface I { @pure fn f(self); } class C implements I { pub fn f(self) { println(1); } } fn main() {}",
            false,
            "E2620",
        ),
        ("@pure fn f() { println(1); } fn main() {}", false, "E2620"),
    ] {
        let fixture = Fixture::new(source);
        for command in ["check", "check"] {
            let output = fixture.command(command);
            let text = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.success(), accepted, "{source}: {text}");
            if !accepted {
                assert!(text.contains(code), "{text}");
            }
        }
    }
}

#[test]
fn purity_driver_example_runs() {
    let fixture = Fixture::new(include_str!("../example/pure_functions.wi"));
    let output = fixture.command("run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|s| s == "30")
    );
}

#[test]
fn effect_contract_check_and_build_reject_and_recheck() {
    let fixture = Fixture::new(
        "fn helper(c: BlockingCell<i64>) { c.get(); } @effects(no_wait) fn guarded(c: BlockingCell<i64>) { helper(c); } fn main() {}",
    );
    for command in ["check", "build", "check"] {
        let out = fixture.command(command);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!out.status.success(), "{command}: {text}");
        for expected in [
            "E2610",
            "no_wait",
            "BlockingCell.get",
            "compiler-proven",
            "guarded",
            "helper",
        ] {
            assert!(
                text.contains(expected),
                "{command}: missing {expected}: {text}"
            );
        }
    }
    fs::write(
        fixture.0.join("main.wi"),
        "@effects(no_wait) fn guarded() -> i64 { return 42; } fn main() { println(guarded()); }",
    )
    .unwrap();
    let out = fixture.command("check");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn effect_contract_generic_template_and_instance() {
    for source in [
        "fn helper<T>(x: T, c: BlockingCell<i64>) { c.get(); } @effects(no_wait) fn f<T>(x: T, c: BlockingCell<i64>) { helper(x, c); } fn main() {}",
        "class C<T> { @effects(no_wait) pub fn f(self, c: BlockingCell<i64>) { c.get(); } } fn main() {}",
        "@effects(no_wait) fn f<T>(x: T, c: BlockingCell<i64>) { c.get(); } fn main() {}",
        "fn helper(c: BlockingCell<i64>) { c.get(); } @effects(no_wait) fn f<T>(x: T, c: BlockingCell<i64>) { helper(c); } fn main() {}",
        "@effects(no_wait) fn f<T>(x: T, c: BlockingCell<i64>) { c.get(); } fn main() { f(1, BlockingCell::new(0)); }",
    ] {
        let fixture = Fixture::new(source);
        let out = fixture.command("check");
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("E2610"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn effect_contract_example_runs() {
    let fixture = Fixture::new(include_str!("../example/effect_contracts.wi"));
    let out = fixture.command("run");
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line == "42")
    );
}

#[test]
fn effect_contract_unannotated_execution_is_unchanged() {
    let source = include_str!("../example/effect_contracts.wi");
    let plain = source
        .lines()
        .filter(|line| {
            !line.starts_with("@effects(") && !line.trim_start().starts_with("@effects(")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let fixture = Fixture::new(&plain);
    let out = fixture.command("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line == "42")
    );
}

#[test]
fn effect_contract_mixed_waits_survive_driver_body_queries() {
    for reverse in [false, true] {
        for (contract, root) in [("no_suspend", "await"), ("no_block", "BlockingCell.get")] {
            let body = if reverse {
                "await sleep(1); c.get();"
            } else {
                "c.get(); await sleep(1);"
            };
            let fixture = Fixture::new(&format!(
                "@effects({contract}) async fn f(c: BlockingCell<i64>) {{ {body} }} fn main() {{}}"
            ));
            for command in ["check", "check", "build"] {
                let output = fixture.command(command);
                let text = String::from_utf8_lossy(&output.stderr);
                assert!(!output.status.success(), "{contract}: {text}");
                assert!(
                    text.contains("E2610") && text.contains(&format!("root operation `{root}`")),
                    "{text}"
                );
            }
            let body = if reverse {
                "ch.recv(); c.get();"
            } else {
                "c.get(); ch.recv();"
            };
            let root = if contract == "no_suspend" {
                "Channel.recv"
            } else {
                root
            };
            let fixture = Fixture::new(&format!(
                "fn leaf(c: BlockingCell<i64>, ch: Channel<i64>) {{ {body} }} fn middle(c: BlockingCell<i64>, ch: Channel<i64>) {{ leaf(c, ch); }} @effects({contract}) fn f(c: BlockingCell<i64>, ch: Channel<i64>) {{ middle(c, ch); }} fn main() {{}}"
            ));
            let output = fixture.command("check");
            let text = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success());
            assert!(
                text.contains("E2610")
                    && text.contains(&format!("root operation `{root}`"))
                    && text.contains("f -> middle -> leaf"),
                "{text}"
            );
        }
    }
}

#[test]
fn effect_contract_generic_interface_defaults_check_and_build() {
    for declaration in [
        "interface I<T> { @effects(no_wait) fn f(self, c: BlockingCell<i64>) { c.get(); } }",
        "fn leaf(c: BlockingCell<i64>) { c.get(); } fn middle(c: BlockingCell<i64>) { leaf(c); } interface I<T> { @effects(no_wait) fn f(self, c: BlockingCell<i64>) { middle(c); } }",
        "interface I<T> { fn helper(self, c: BlockingCell<i64>) { c.get(); } @effects(no_wait) fn f(self, c: BlockingCell<i64>) { self.helper(c); } }",
    ] {
        for implementation in ["", "class C implements I<i64> {}"] {
            let fixture = Fixture::new(&format!("{declaration} {implementation} fn main() {{}}"));
            for command in ["check", "build"] {
                let output = fixture.command(command);
                let text = String::from_utf8_lossy(&output.stderr);
                assert!(!output.status.success(), "{command}: {text}");
                assert!(
                    text.contains("E2610") && text.contains("root operation `BlockingCell.get`"),
                    "{command}: {text}"
                );
            }
        }
    }
    let fixture = Fixture::new(
        "interface I<T> { @effects(no_wait) fn f(self, x: T) -> T { return x; } } fn main() {}",
    );
    let output = fixture.command("check");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn effect_contract_polymorphic_module_revisions() {
    let fixture = Fixture::new(
        "import worker; @effects(no_wait) fn guard(c: BlockingCell<i64>) { worker::run(c); } fn main() {}",
    );
    let module = fixture.0.join("worker.wi");
    for (body, expected) in [
        ("pub fn run(c: BlockingCell<i64>) {}", None),
        (
            "pub fn run(c: BlockingCell<i64>) { c.get(); }",
            Some("E2610"),
        ),
        (
            "extern rust { fn foreign(); } pub fn run(c: BlockingCell<i64>) { foreign(); }",
            Some("E2612"),
        ),
        ("pub fn run(c: BlockingCell<i64>) {}", None),
    ] {
        fs::write(&module, body).unwrap();
        for _ in 0..2 {
            let out = fixture.command("check");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.success(), expected.is_none(), "{body}: {stderr}");
            if let Some(code) = expected {
                assert!(stderr.contains(code), "{stderr}");
            }
        }
    }
}

#[test]
fn effect_contract_polymorphic_imported_obligations() {
    let fixture = Fixture::new(
        "import worker; class C implements worker::I { pub fn run(self, c: BlockingCell<i64>) { c.get(); } } fn main() {}",
    );
    fs::write(
        fixture.0.join("worker.wi"),
        "pub interface I { @effects(no_wait) fn run(self, c: BlockingCell<i64>); }",
    )
    .unwrap();
    for command in ["check", "build"] {
        let out = fixture.command(command);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && stderr.contains("E2610"),
            "{command}: {stderr}"
        );
    }
    fs::write(
        fixture.0.join("worker.wi"),
        "pub open class B { @effects(no_wait) pub open fn run(self) {} }",
    )
    .unwrap();
    fs::write(fixture.0.join("main.wi"), "import worker; class C extends worker::B { @effects(no_suspend) pub override fn run(self) {} } fn main() {}").unwrap();
    let out = fixture.command("check");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && stderr.contains("E2611"),
        "{stderr}"
    );
}

#[test]
fn effect_contract_polymorphic_generics_and_defaults() {
    for (source, expected) in [
        (
            "interface I { @effects(no_wait) fn run(self, c: BlockingCell<i64>); } interface J<T> extends I { fn run(self, c: BlockingCell<i64>) { c.get(); } } fn main() {}",
            Some("E2610"),
        ),
        (
            "interface I { @effects(no_wait) fn run(self, c: BlockingCell<i64>); } class C<T> implements I { pub fn run(self, c: BlockingCell<i64>) { c.get(); } } fn main() {}",
            Some("E2610"),
        ),
        (
            "interface I<T> { @effects(no_wait) fn run(self, c: BlockingCell<i64>); } class C implements I<i64> { pub fn run(self, c: BlockingCell<i64>) { c.get(); } } fn main() {}",
            Some("E2610"),
        ),
        (
            "interface I { @effects(no_wait) fn run(self) {} } class C implements I {} @effects(no_wait) fn guard(c: C) { c.run(); } fn main() {}",
            None,
        ),
        (
            "interface I { @effects(no_wait) fn run(self); } interface J extends I { @effects(no_suspend) fn run(self); } fn main() {}",
            Some("E2611"),
        ),
        (
            "extern rust { @effects(no_wait) fn foreign(); } @effects(no_wait) fn guard() { foreign(); } fn main() {}",
            Some("E0105"),
        ),
    ] {
        let fixture = Fixture::new(source);
        let out = fixture.command("check");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.success(),
            expected.is_none(),
            "{source}: {stderr}"
        );
        if let Some(code) = expected {
            assert!(stderr.contains(code), "{source}: {stderr}");
        }
    }
}

#[test]
fn effect_contract_polymorphic_example_runs() {
    let fixture = Fixture::new(include_str!("../example/effect_contract_dispatch.wi"));
    let out = fixture.command("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line.trim() == "42")
    );
}

#[test]
fn effect_contract_polymorphic_imported_virtual_caller() {
    let fixture = Fixture::new(
        "import worker; class C extends worker::B { pub override fn run(self, c: BlockingCell<i64>) { c.get(); } } fn main() { worker::guard(new C(), BlockingCell::new(0)); }",
    );
    fs::write(fixture.0.join("worker.wi"), "pub open class B { pub open fn run(self, c: BlockingCell<i64>) {} } @effects(no_wait) pub fn guard(b: B, c: BlockingCell<i64>) { b.run(c); }").unwrap();
    let out = fixture.command("check");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && (stderr.contains("E2610") || stderr.contains("E2612")),
        "{stderr}"
    );
}

#[test]
fn io_contract_driver_imports_builtins_and_cache() {
    let fixture =
        Fixture::new("import worker; @effects(no_io) fn guard() { worker::run(); } fn main() {}");
    for body in ["println(1);", "", "println(2);"] {
        fs::write(
            fixture.0.join("worker.wi"),
            format!("pub fn run() {{ {body} }}"),
        )
        .unwrap();
        for _ in 0..2 {
            let output = fixture.command("check");
            let text = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.success(), body.is_empty(), "{text}");
            if !body.is_empty() {
                assert!(
                    text.contains("E2610") && text.contains("worker::run"),
                    "{text}"
                );
            }
        }
    }
    for (import, call) in [
        ("import std::fs;", "fs::exists(\"missing\");"),
        ("import std::fs as files;", "files::exists(\"missing\");"),
        ("import std::fs;", "await fs::exists_async(\"missing\");"),
        ("import std::net;", "net::bind(\"127.0.0.1:0\");"),
        (
            "import std::net;",
            "await net::connect_async(\"127.0.0.1:1\");",
        ),
    ] {
        fs::write(
            fixture.0.join("main.wi"),
            format!("{import} @effects(no_io) async fn guard() {{ {call} }} fn main() {{}}"),
        )
        .unwrap();
        let output = fixture.command("check");
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && text.contains("E2610"),
            "{call}: {text}"
        );
    }
}

#[test]
fn io_contract_example_runs() {
    let fixture = Fixture::new(include_str!("../example/io_contracts.wi"));
    let output = fixture.command("run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("value=42"));
}

#[test]
fn io_contract_driver_inherited_default_and_generic_body() {
    for source in [
        "interface I { fn out(self) { println(1); } } class C implements I {} @effects(no_io) fn f(c:C) { c.out(); } fn main() {}",
        "fn out<T>(x:T) { println(1); } @effects(no_io) fn f() { out(1); } fn main() {}",
    ] {
        let fixture = Fixture::new(source);
        let output = fixture.command("check");
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && text.contains("E2610") && text.contains("root operation"),
            "{text}"
        );
    }
}

#[test]
fn panic_allocation_imported_contracts() {
    for (contract, body, root) in [
        ("no_panic", "panic(\"bad\");", "panic"),
        ("no_alloc", "let a=[1];", "allocation"),
    ] {
        let fixture = Fixture::new(&format!(
            "import worker; @effects({contract}) fn guard() {{ worker::middle(); }} fn main() {{}}"
        ));
        fs::write(
            fixture.0.join("worker.wi"),
            format!("fn leaf() {{ {body} }} pub fn middle() {{ leaf(); }}"),
        )
        .unwrap();
        for command in ["check", "build", "check"] {
            let out = fixture.command(command);
            let text = String::from_utf8_lossy(&out.stderr);
            assert!(
                !out.status.success() && text.contains("E2610"),
                "{command}: {text}"
            );
            assert!(
                text.contains("guard")
                    && text.contains("middle")
                    && text.contains(root)
                    && text.contains("call chain"),
                "{text}"
            );
        }
        fs::write(fixture.0.join("worker.wi"), "pub fn middle() {}").unwrap();
        let out = fixture.command("check");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn panic_allocation_example_runs() {
    let out = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["run", "example/panic_allocation_contracts.wi"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line == "42")
    );
}

#[test]
fn panic_allocation_checked_profiles() {
    let fixture = Fixture::new(
        "fn helper(x:i64)->i64 { return x+1; } @effects(no_panic) fn guard(x:i64)->i64 { return helper(x); } fn main() {}",
    );
    for profile in ["--debug", "--release"] {
        let out = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&fixture.0)
            .args(["build", "main.wi", profile])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && text.contains("E2610") && text.contains("guard -> helper"),
            "{profile}: {text}"
        );
    }
}

#[test]
fn panic_allocation_imported_string_constant() {
    for expression in ["worker::MESSAGE", "MESSAGE"] {
        let import = if expression == "MESSAGE" {
            "import worker::MESSAGE;"
        } else {
            "import worker;"
        };
        let fixture = Fixture::new(&format!(
            "{import} @effects(no_alloc) fn value()->String {{ return {expression}; }} fn main() {{}}"
        ));
        fs::write(
            fixture.0.join("worker.wi"),
            "pub const MESSAGE:String=\"hello\";",
        )
        .unwrap();
        let out = fixture.command("check");
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && text.contains("E2610"),
            "{expression}: {text}"
        );
    }
}

#[test]
fn panic_allocation_imported_dispatch_and_obligations() {
    for (contract, other, body) in [
        ("no_panic", "no_alloc", "panic(\"bad\");"),
        ("no_alloc", "no_panic", "let a=[1];"),
    ] {
        let fixture = Fixture::new(&format!(
            "import worker; class C implements worker::I {{ pub fn run(self) {{ {body} }} }} @effects({contract}) fn guard(i:worker::I) {{ i.run(); }} fn main() {{}}"
        ));
        fs::write(
            fixture.0.join("worker.wi"),
            "pub interface I { fn run(self); }",
        )
        .unwrap();
        let out = fixture.command("check");
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success()
                && text.contains("E2610")
                && text.contains("guard")
                && text.contains("call chain"),
            "{text}"
        );
        fs::write(
            fixture.0.join("worker.wi"),
            format!("pub interface I {{ @effects({contract}) fn run(self); }}"),
        )
        .unwrap();
        let out = fixture.command("check");
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && text.contains("E2610"), "{text}");
        fs::write(fixture.0.join("main.wi"), format!("import worker; class C implements worker::I {{ @effects({other}) pub fn run(self) {{}} }} fn main() {{}}")).unwrap();
        let out = fixture.command("check");
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && text.contains("E2611"), "{text}");
    }
}

#[test]
fn no_alloc_imported_ranges_and_typed_storage() {
    for (params, args, body) in [
        ("", "", "let r=0..10;"),
        ("c:BlockingCell<I>,v:I", "c,v", "c.set(v);"),
        (
            "c:BlockingCell<Option<i64>>,v:Option<i64>",
            "c,v",
            "c.set(v);",
        ),
        (
            "c:BlockingRwCell<Result<i64,bool>>,v:Result<i64,bool>",
            "c,v",
            "c.write(v);",
        ),
    ] {
        let source = format!(
            "import worker; import worker::I; @effects(no_alloc) fn guard({params}) {{ worker::helper({args}); }} fn main() {{}}"
        );
        let fixture = Fixture::new(&source);
        fs::write(
            fixture.0.join("worker.wi"),
            format!("pub interface I extends Sync {{}} pub fn helper({params}) {{ {body} }}"),
        )
        .unwrap();
        for command in ["check", "build", "check"] {
            let output = fixture.command(command);
            let text = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success()
                    && text.contains("E2610")
                    && text.contains("allocation")
                    && text.contains("guard -> worker::helper"),
                "{body} {command}: {text}"
            );
        }
    }
}

#[test]
fn no_alloc_generic_typed_storage_helper() {
    let fixture = Fixture::new(
        "interface I extends Sync {} @effects(no_alloc) fn guard(c:BlockingCell<I>,v:I) { helper(0,c,v); } fn helper<T>(tag:T,c:BlockingCell<I>,v:I) { c.set(v); } fn main() {}",
    );
    for command in ["check", "build"] {
        let output = fixture.command(command);
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success()
                && text.contains("E2610")
                && text.contains("guard")
                && text.contains("helper"),
            "{command}: {text}"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines, reason = "split pending")]
fn no_alloc_enum_representations_and_propagation() {
    for (label, declarations, params, result, body, allocates) in [
        (
            "tag",
            "enum Color { Red, Blue }",
            "",
            "Color",
            "return Color::Red;",
            false,
        ),
        (
            "discarded tag",
            "enum Color { Red, Blue }",
            "",
            "Color",
            "Color::Blue; return Color::Red;",
            false,
        ),
        (
            "bare tag",
            "enum Color { Red, Blue }",
            "",
            "Color",
            "return Red;",
            false,
        ),
        (
            "scalar some",
            "",
            "",
            "Option<i64>",
            "return Some(1);",
            false,
        ),
        ("scalar none", "", "", "Option<i64>", "return None;", false),
        (
            "float some",
            "",
            "",
            "Option<f64>",
            "return Some(1.5);",
            false,
        ),
        (
            "bool some",
            "",
            "",
            "Option<bool>",
            "return Some(true);",
            false,
        ),
        (
            "scalar ok",
            "",
            "",
            "Result<i64,bool>",
            "return Ok(1);",
            false,
        ),
        (
            "scalar err",
            "",
            "",
            "Result<i64,bool>",
            "return Err(false);",
            false,
        ),
        (
            "nullable some",
            "",
            "s:String",
            "Option<String>",
            "return Some(s);",
            false,
        ),
        (
            "nullable none",
            "",
            "",
            "Option<String>",
            "return None;",
            false,
        ),
        (
            "nullable class",
            "class C {}",
            "c:C",
            "Option<C>",
            "return Some(c);",
            false,
        ),
        (
            "boxed variant",
            "enum E { A(i64), B }",
            "",
            "E",
            "return E::A(1);",
            true,
        ),
        (
            "boxed empty variant",
            "enum E { A(i64), B }",
            "",
            "E",
            "return E::B;",
            true,
        ),
        (
            "nested option",
            "",
            "x:Option<i64>",
            "Option<Option<i64>>",
            "return Some(x);",
            true,
        ),
        (
            "nested none",
            "",
            "",
            "Option<Option<i64>>",
            "return None;",
            true,
        ),
        (
            "boxed result",
            "",
            "s:String",
            "Result<String,bool>",
            "return Ok(s);",
            true,
        ),
        (
            "allocating payload",
            "",
            "",
            "Option<String>",
            "return Some(\"hello\");",
            true,
        ),
        (
            "pair propagation",
            "",
            "x:Result<i64,bool>",
            "Result<i64,bool>",
            "x?; return x;",
            false,
        ),
        (
            "boxed propagation",
            "",
            "x:Result<String,bool>",
            "Result<String,bool>",
            "x?; return x;",
            false,
        ),
        (
            "boxed rebind",
            "",
            "x:Result<String,bool>,y:Result<Option<i64>,bool>",
            "Result<Option<i64>,bool>",
            "x?; return y;",
            false,
        ),
        (
            "pair to box",
            "",
            "x:Result<i64,bool>,y:Result<String,bool>",
            "Result<String,bool>",
            "x?; return y;",
            true,
        ),
        (
            "box to pair",
            "",
            "x:Result<String,bool>,y:Result<i64,bool>",
            "Result<i64,bool>",
            "x?; return y;",
            false,
        ),
        (
            "option pair propagation",
            "",
            "x:Option<i64>",
            "Option<i64>",
            "x?; return x;",
            false,
        ),
        (
            "option nullable propagation",
            "",
            "x:Option<String>",
            "Option<String>",
            "x?; return x;",
            false,
        ),
        (
            "option boxed propagation",
            "",
            "x:Option<Option<i64>>",
            "Option<Option<i64>>",
            "x?; return x;",
            true,
        ),
        (
            "option to nullable",
            "",
            "x:Option<i64>,y:Option<String>",
            "Option<String>",
            "x?; return y;",
            false,
        ),
        (
            "option to box",
            "",
            "x:Option<i64>,y:Option<Option<i64>>",
            "Option<Option<i64>>",
            "x?; return y;",
            true,
        ),
    ] {
        let fixture = Fixture::new(&format!(
            "{declarations} @effects(no_alloc) fn f({params})->{result} {{ {body} }} fn main() {{}}"
        ));
        for command in ["check", "build"] {
            let out = fixture.command(command);
            let text = String::from_utf8_lossy(&out.stderr);
            assert_eq!(
                out.status.success(),
                !allocates,
                "{label} {command}: {text}"
            );
            if allocates {
                assert!(text.contains("E2610"), "{label}: {text}");
            }
        }
    }
}

#[test]
fn no_alloc_propagation_conversion_effects() {
    for (label, declarations, target, allocates) in [
        (
            "pure scalar conversion",
            "class Source implements Into<i64> { pub fn into(self)->i64 { return 7; } }",
            "i64",
            false,
        ),
        (
            "allocating scalar conversion",
            "class Source implements Into<i64> { pub fn into(self)->i64 { let a=[1]; return 7; } }",
            "i64",
            true,
        ),
        (
            "virtual scalar conversion",
            "open class Source implements Into<i64> { pub open fn into(self)->i64 { return 7; } } class Child extends Source { pub override fn into(self)->i64 { let a=[1]; return 8; } }",
            "i64",
            true,
        ),
        (
            "boxed converted result",
            "class Dest {} class Source implements Into<Dest> { pub dest:Dest; pub fn into(self)->Dest { return self.dest; } }",
            "Dest",
            true,
        ),
    ] {
        for helper in [false, true] {
            let functions = if helper {
                format!(
                    "@effects(no_alloc) fn f(x:Result<i64,Source>,y:Result<i64,{target}>)->Result<i64,{target}> {{ return helper(x,y); }} fn helper(x:Result<i64,Source>,y:Result<i64,{target}>)->Result<i64,{target}> {{ x?; return y; }}"
                )
            } else {
                format!(
                    "@effects(no_alloc) fn f(x:Result<i64,Source>,y:Result<i64,{target}>)->Result<i64,{target}> {{ x?; return y; }}"
                )
            };
            let fixture = Fixture::new(&format!("{declarations} {functions} fn main() {{}}"));
            for command in ["check", "build"] {
                let out = fixture.command(command);
                let text = String::from_utf8_lossy(&out.stderr);
                assert_eq!(
                    out.status.success(),
                    !allocates,
                    "{label} helper={helper} {command}: {text}"
                );
                if allocates {
                    assert!(text.contains("E2610"), "{label}: {text}");
                }
            }
        }
    }
}

#[test]
fn no_alloc_imported_enum_propagation() {
    for allocates in [false, true] {
        let result = if allocates {
            "Result<String,bool>"
        } else {
            "Result<i64,bool>"
        };
        let fixture = Fixture::new(&format!(
            "import worker; @effects(no_alloc) fn f(x:Result<i64,bool>,y:{result})->{result} {{ return worker::helper(x,y); }} fn main() {{}}"
        ));
        fs::write(
            fixture.0.join("worker.wi"),
            format!("pub fn helper(x:Result<i64,bool>,y:{result})->{result} {{ x?; return y; }}"),
        )
        .unwrap();
        for command in ["check", "build", "check"] {
            let out = fixture.command(command);
            let text = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.success(), !allocates, "{command}: {text}");
            if allocates {
                assert!(
                    text.contains("E2610") && text.contains("f -> worker::helper"),
                    "{text}"
                );
            }
        }
    }
}

#[test]
fn no_alloc_imported_enum_and_conversion_dispatch() {
    for (provider, params, result, body, allocates) in [
        (
            "pub enum Color { Red, Blue }",
            "",
            "worker::Color",
            "return worker::Color::Red;",
            false,
        ),
        (
            "pub class Source implements Into<i64> { pub fn into(self)->i64 { return 1; } }",
            "x:Result<i64,worker::Source>,y:Result<i64,i64>",
            "Result<i64,i64>",
            "x?; return y;",
            false,
        ),
        (
            "pub class Source implements Into<i64> { pub fn into(self)->i64 { let a=[1]; return 1; } }",
            "x:Result<i64,worker::Source>,y:Result<i64,i64>",
            "Result<i64,i64>",
            "x?; return y;",
            true,
        ),
    ] {
        let fixture = Fixture::new(&format!(
            "import worker; @effects(no_alloc) fn f({params})->{result} {{ {body} }} fn main() {{}}"
        ));
        fs::write(fixture.0.join("worker.wi"), provider).unwrap();
        for command in ["check", "build", "check"] {
            let out = fixture.command(command);
            let text = String::from_utf8_lossy(&out.stderr);
            assert_eq!(
                out.status.success(),
                !allocates,
                "{provider} {command}: {text}"
            );
            if allocates {
                assert!(text.contains("E2610") && text.contains("into"), "{text}");
            }
            assert!(
                !text.contains("$contract$into$"),
                "synthetic node exposed: {text}"
            );
        }
    }
}

#[test]
fn no_alloc_interface_option_boxes() {
    for (declaration, payload) in [
        ("pub interface I {}", "I"),
        ("pub interface I<T> {}", "I<i64>"),
    ] {
        for body in ["return Some(x);", "return None;", "v?; return v;"] {
            for route in ["direct", "helper", "imported"] {
                let params = format!("x:{payload},v:Option<{payload}>");
                let result = format!("Option<{payload}>");
                let source = match route {
                    "direct" => format!(
                        "{declaration} @effects(no_alloc) fn guard({params})->{result} {{ {body} }} fn main() {{}}"
                    ),
                    "helper" => format!(
                        "{declaration} fn helper({params})->{result} {{ {body} }} @effects(no_alloc) fn guard({params})->{result} {{ return helper(x,v); }} fn main() {{}}"
                    ),
                    _ => format!(
                        "import worker; import worker::I; @effects(no_alloc) fn guard({params})->{result} {{ return worker::helper(x,v); }} fn main() {{}}"
                    ),
                };
                let fixture = Fixture::new(&source);
                if route == "imported" {
                    fs::write(
                        fixture.0.join("worker.wi"),
                        format!("{declaration} pub fn helper({params})->{result} {{ {body} }}"),
                    )
                    .unwrap();
                }
                for command in ["check", "build", "check"] {
                    let output = fixture.command(command);
                    let text = String::from_utf8_lossy(&output.stderr);
                    let chain = match route {
                        "direct" => "guard -> allocation",
                        "helper" => "guard -> helper -> allocation",
                        _ => "guard -> worker::helper -> allocation",
                    };
                    assert!(
                        !output.status.success() && text.contains("E2610") && text.contains(chain),
                        "{payload} {body} {route} {command}: {text}"
                    );
                }
            }
        }
    }
}

#[test]
fn no_alloc_map_iteration_snapshot() {
    for (ty, body, allocates) in [
        ("Map<i64,i64>", "for item in value {}", true),
        ("Map<String,bool>", "for item in value { break; }", true),
        ("Array<i64>", "for item in value {}", false),
        ("Range<i64>", "for item in value {}", false),
        ("i64", "for item in 0..value {}", false),
    ] {
        for route in ["direct", "helper", "imported"] {
            let imports = "import std::collections::Map; import std::collections::Array;";
            let source = match route {
                "direct" => format!(
                    "{imports} @effects(no_alloc) fn guard(value:{ty}) {{ {body} }} fn main() {{}}"
                ),
                "helper" => format!(
                    "{imports} fn helper(value:{ty}) {{ {body} }} @effects(no_alloc) fn guard(value:{ty}) {{ helper(value); }} fn main() {{}}"
                ),
                _ => format!(
                    "{imports} import worker; @effects(no_alloc) fn guard(value:{ty}) {{ worker::helper(value); }} fn main() {{}}"
                ),
            };
            let fixture = Fixture::new(&source);
            if route == "imported" {
                fs::write(
                    fixture.0.join("worker.wi"),
                    format!("{imports} pub fn helper(value:{ty}) {{ {body} }}"),
                )
                .unwrap();
            }
            for command in ["check", "build", "check"] {
                let output = fixture.command(command);
                let text = String::from_utf8_lossy(&output.stderr);
                assert_eq!(
                    output.status.success(),
                    !allocates,
                    "{ty} {route} {command}: {text}"
                );
                if allocates {
                    let chain = match route {
                        "direct" => "guard -> allocation",
                        "helper" => "guard -> helper -> allocation",
                        _ => "guard -> worker::helper -> allocation",
                    };
                    assert!(
                        text.contains("E2610") && text.contains(chain),
                        "{ty} {route} {command}: {text}"
                    );
                }
            }
        }
    }
}

#[test]
fn panic_allocation_generic_error_conversion() {
    for generic in [false, true] {
        let (decl, ty) = if generic {
            (
                "class Wrap<T> implements Into<Other> { pub o: Other; pub v: T;",
                "Wrap<i64>",
            )
        } else {
            (
                "class Wrap implements Into<Other> { pub o: Other; pub v: i64;",
                "Wrap",
            )
        };
        for (contract, into, chain) in [
            ("no_alloc", "return new Other(1);", Some("g -> allocation")),
            (
                "no_panic",
                "panic(\"boom\");",
                Some("g -> Wrap::into -> panic"),
            ),
            ("no_panic", "return self.o;", None),
        ] {
            let source = format!(
                "interface Into<T> {{ fn into(self) -> T; }} class Other {{ pub v: i64; }} {decl} pub fn into(self) -> Other {{ {into} }} }} @effects({contract}) fn g(x: Result<i64, {ty}>, y: Result<i64, Other>) -> Result<i64, Other> {{ x?; return y; }} fn main() {{}}"
            );
            let output = Fixture::new(&source).command("check");
            let text = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                output.status.success(),
                chain.is_none(),
                "{ty} {contract}: {text}"
            );
            if let Some(chain) = chain {
                assert!(
                    text.contains("E2610") && text.contains(chain),
                    "{ty} {contract}: {text}"
                );
            }
        }
    }
}

#[test]
fn no_alloc_async_call_through_dispatch() {
    for (receiver, overridden) in [("Base", true), ("D", true), ("Base", false)] {
        let (modifier, body) = if overridden {
            (
                "open",
                "pub override async fn run(self) -> i64 { return 2; }",
            )
        } else {
            ("", "")
        };
        let source = format!(
            "open class Base {{ pub {modifier} async fn run(self) -> i64 {{ return 1; }} }} class D extends Base {{ {body} }} @effects(no_alloc) fn f(x: {receiver}) {{ let _ = x.run(); }} @effects(no_alloc) fn sync_ok(x: {receiver}) {{ }} fn main() {{}}"
        );
        for command in ["check", "build"] {
            let output = Fixture::new(&source).command(command);
            let text = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success()
                    && text.contains("E2610")
                    && text.contains("f -> allocation")
                    && !text.contains("violated by `sync_ok`"),
                "{receiver} {overridden} {command}: {text}"
            );
        }
    }
}

#[test]
fn contract_diagnostics_locate_offset_witnesses() {
    for (source, line, chain) in [
        (
            "import std::collections::Map;\n@effects(no_alloc) fn f(m: Map<i64,i64>) {\n  for i in m {}\n}\nfn main() {}",
            "main.wi:3:3",
            "f -> allocation",
        ),
        (
            "async fn h() -> i64 { return 1; }\n@effects(no_alloc) fn k() {\n  let _ = h();\n}\nfn main() {}",
            "main.wi:3:",
            "k -> allocation",
        ),
        (
            "fn boom() { panic(\"x\"); }\n@effects(no_panic) fn p() {\n  boom();\n}\nfn main() {}",
            "main.wi:",
            "p -> boom -> panic",
        ),
    ] {
        let output = Fixture::new(source).command("check");
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success()
                && text.contains("E2610")
                && text.contains(chain)
                && text.contains(line)
                && !text.contains("main.wi:0:0"),
            "{source}: {text}"
        );
    }
}

#[test]
fn contract_injected_defaults_and_enum_members_fail_closed() {
    for (source, ok) in [
        // A class copy of an interface default is not re-scanned with the
        // copy's missing checked types; the canonical default decides.
        (
            "enum Result<T,E> { Ok(T), Err(E) } interface I { fn m(self, x: Result<i64,i64>) -> Result<i64,i64> { x?; return x; } } class C implements I {} @effects(no_alloc) fn f(c: C, x: Result<i64,i64>) { let _ = c.m(x); } fn main() {}",
            true,
        ),
        (
            "interface I { fn m(self) -> String { return \"s\"; } } class C implements I {} @effects(no_alloc) fn f(c: C) { let _ = c.m(); } fn main() {}",
            false,
        ),
        // Variants are values; a non-variant member is not silently dropped.
        (
            "enum Color { Red, Green(i64) } @effects(no_panic) fn f() -> Color { return Color::Red; } @effects(no_panic) fn g() -> Color { return Color::Green(1); } fn main() {}",
            true,
        ),
    ] {
        let output = Fixture::new(source).command("check");
        let text = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), ok, "{source}: {text}");
        if !ok {
            assert!(
                text.contains("E2610") || text.contains("E2612"),
                "{source}: {text}"
            );
        }
    }
}

#[test]
fn no_alloc_fault_reporting_and_proof_only_clauses() {
    let prelude = "import std::collections::Array;";
    for (body, contract, expected) in [
        // Allocation only on the fault path (message of the raised panic).
        (
            "fn f(a: FrozenArray<i64>) -> i64 { return a.len(); }",
            "no_alloc",
            None,
        ),
        (
            "fn f(a: Array<i64>) -> i64 { return a.len(); }",
            "no_alloc",
            None,
        ),
        (
            "fn f(a: Array<i64>) -> i64 { return a[0]; }",
            "no_alloc",
            None,
        ),
        (
            "fn f(s: String) -> i64 { return s.byte_at(0); }",
            "no_alloc",
            None,
        ),
        (
            "fn f(o: Option<i64>) -> i64 { return o.unwrap(); }",
            "no_alloc",
            None,
        ),
        // The fault itself is still a panic.
        (
            "fn f(a: Array<i64>) -> i64 { return a.len(); }",
            "no_panic",
            Some("E2610"),
        ),
        (
            "fn f(o: Option<i64>) -> i64 { return o.unwrap(); }",
            "no_panic",
            Some("E2610"),
        ),
        // Success-path allocation and the expect message literal still count.
        (
            "fn f(s: String) -> String { return s + s; }",
            "no_alloc",
            Some("E2610"),
        ),
        (
            "fn f(o: Option<i64>) -> i64 { return o.expect(\"x\"); }",
            "no_alloc",
            Some("E2610"),
        ),
        // Loop invariants/variants are dropped by lowering.
        (
            "fn f(a: FrozenArray<i64>) -> i64 { let mut t = 0; for v in a invariant t >= 0 && t <= a.len() variant a.len() - t { t = t + 1; } return t; }",
            "no_alloc",
            None,
        ),
        (
            "fn f(a: FrozenArray<i64>) -> i64 { let mut t = 0; for v in a invariant t <= a.len() { t = 1; } return t; }",
            "no_panic",
            None,
        ),
        (
            "fn f(n: i64) -> i64 { let mut i = 0; while i < n invariant i >= 0 variant n - i { i = i + 1; } return i; }",
            "no_alloc",
            None,
        ),
        // Checked arithmetic in a variant is not a runtime panic site; the
        // same arithmetic in the loop body still is.
        (
            "fn f(n: i64) { let mut i = 0; while i < n invariant i >= 0 variant n - i { break; } }",
            "no_panic",
            None,
        ),
        (
            "fn f(n: i64) { let mut i = 0; while i < n invariant i >= 0 variant n - i { i = i + 1; } }",
            "no_panic",
            Some("E2610"),
        ),
        // Generic Option payloads may need the boxed representation.
        (
            "fn f<T>(x: T) -> Option<T> { return Some(x); }",
            "no_alloc",
            Some("E2610"),
        ),
        (
            "fn f(x: String) -> Option<String> { return Some(x); }",
            "no_alloc",
            None,
        ),
    ] {
        let source = format!("{prelude} @effects({contract}) {body} fn main() {{}}");
        let output = Fixture::new(&source).command("check");
        let text = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.success(),
            expected.is_none(),
            "{source}: {text}"
        );
        if let Some(code) = expected {
            assert!(text.contains(code), "{source}: {text}");
        }
    }
}
