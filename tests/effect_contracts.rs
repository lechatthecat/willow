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
