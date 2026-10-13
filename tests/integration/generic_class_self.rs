//! Generic class receiver identity (willow-sfab).
use super::support::*;

fn runs(files: &[(&str, &str)], expected: &str) {
    let project = TestProject::new("generic_class_self", files);
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let output = project.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
}

#[test]
fn explicit_return() {
    runs(
        &[(
            "main.wi",
            "class C2<T> { pub value: T; pub fn me(self) -> C2<T> { return self; } } fn main() { let d: C2<i64> = new C2<i64>(5).me(); println(d.value); }",
        )],
        "5\n",
    );
}

#[test]
fn conformance() {
    runs(
        &[(
            "main.wi",
            "interface P { fn me(self) -> Self; } class C2<T> implements P { pub value: T; pub fn me(self) -> C2<T> { return self; } } fn main() { println(new C2<i64>(5).me().value); }",
        )],
        "5\n",
    );
}

#[test]
fn default_return() {
    runs(
        &[(
            "main.wi",
            "interface P { fn me(self) -> Self { return self; } } class C2<T> implements P { pub value: T; } fn main() { println(new C2<i64>(5).me().value); }",
        )],
        "5\n",
    );
}

fn one(source: &str, expected: &str) {
    runs(&[("main.wi", source)], expected);
}

const DEFAULT: &str = "interface P { fn me(self) -> Self { return self; } fn choose(self, other: Self) -> Self { return other; } }";
const CELL: &str = "class Cell<T> implements P { pub value: T; }";

#[test]
fn implicit_receiver() {
    one(
        "class Cell<T> { pub value: T; pub fn me() -> Cell<T> { return self; } } fn main() { println(new Cell<i64>(7).me().value); }",
        "7\n",
    );
}

#[test]
fn inferred_result() {
    one(
        "class Cell<T> { pub value: T; pub fn me(self) -> Cell<T> { return self; } } fn main() { println(new Cell<i64>(8).me().value); }",
        "8\n",
    );
}

#[test]
fn two_instantiations() {
    one(
        &format!(
            "{DEFAULT} {CELL} fn main() {{ println(new Cell<i64>(9).me().value); println(new Cell<String>(\"x\").me().value); }}"
        ),
        "9\nx\n",
    );
}

#[test]
fn self_parameter() {
    one(
        &format!(
            "{DEFAULT} {CELL} fn main() {{ println(new Cell<i64>(1).choose(new Cell<i64>(2)).value); }}"
        ),
        "2\n",
    );
}

#[test]
fn default_chain() {
    one(
        &format!(
            "{DEFAULT} {CELL} fn main() {{ println(new Cell<i64>(3).me().me().me().value); }}"
        ),
        "3\n",
    );
}

#[test]
fn typed_local() {
    one(
        "class Cell<T> { pub value: T; pub fn me(self) -> Cell<T> { let x: Cell<T> = self; return x; } } fn main() { println(new Cell<i64>(4).me().value); }",
        "4\n",
    );
}

#[test]
fn generic_function_argument() {
    one(
        "fn id<T>(x: T) -> T { return x; } class Cell<T> { pub value: T; pub fn me(self) -> Cell<T> { return id(self); } } fn main() { println(new Cell<i64>(5).me().value); }",
        "5\n",
    );
}

#[test]
fn multiple_type_parameters() {
    one(
        &format!(
            "{DEFAULT} class Pair<A, B> implements P {{ pub a: A; pub b: B; }} fn main() {{ let p = new Pair<i64, String>(6, \"six\").me(); println(p.a); println(p.b); }}"
        ),
        "6\nsix\n",
    );
}

#[test]
fn generic_interface_default() {
    one(
        "interface P<T> { fn value(self) -> T; fn me(self) -> Self { return self; } } class Cell<T> implements P<T> { pub v: T; pub fn value(self) -> T { return self.v; } } fn main() { println(new Cell<i64>(7).me().value()); }",
        "7\n",
    );
}

#[test]
fn super_interface_default() {
    one(
        &format!(
            "{DEFAULT} interface Q extends P {{ }} class Cell<T> implements Q {{ pub value: T; }} fn main() {{ println(new Cell<i64>(8).me().value); }}"
        ),
        "8\n",
    );
}

#[test]
fn explicit_override() {
    one(
        &format!(
            "{DEFAULT} class Cell<T> implements P {{ pub value: T; pub fn me(self) -> Cell<T> {{ return self; }} }} fn main() {{ println(new Cell<i64>(9).me().value); }}"
        ),
        "9\n",
    );
}

#[test]
fn nested_self_array() {
    one(
        "import std::collections::Array; interface P { fn all(self) -> Array<Self> { return [self]; } } class Cell<T> implements P { pub value: T; } fn main() { println(new Cell<i64>(10).all()[0].value); }",
        "10\n",
    );
}

#[test]
fn cross_module_plain_default() {
    runs(
        &[
            (
                "p.wi",
                "module p; pub interface P { fn me(self) -> Self { return self; } }",
            ),
            (
                "main.wi",
                "import p; class Cell<T> implements p::P { pub value: T; } fn main() { println(new Cell<i64>(11).me().value); }",
            ),
        ],
        "11\n",
    );
}

#[test]
fn cross_module_generic_default() {
    runs(
        &[
            (
                "p.wi",
                "module p; pub interface P<T> { fn value(self) -> T; fn me(self) -> Self { return self; } }",
            ),
            (
                "main.wi",
                "import p; class Cell<T> implements p::P<T> { pub v: T; pub fn value(self) -> T { return self.v; } } fn main() { println(new Cell<i64>(12).me().value()); }",
            ),
        ],
        "12\n",
    );
}

#[test]
fn imported_generic_class() {
    runs(
        &[
            (
                "p.wi",
                "module p; interface P { fn me(self) -> Self { return self; } } pub class Cell<T> implements P { pub value: T; }",
            ),
            (
                "main.wi",
                "import p; fn main() { println(new p::Cell<i64>(13).me().value); }",
            ),
        ],
        "13\n",
    );
}

#[test]
fn constructor_receiver() {
    one(
        "fn take<T>(c: Cell<T>) -> Cell<T> { return c; } class Cell<T> { pub value: T; pub init(self, value: T) { self.value = value; let x: Cell<T> = take(self); } } fn main() { println(new Cell<i64>(14).value); }",
        "14\n",
    );
}

#[test]
fn wrong_instantiation_rejected() {
    let errors = compile_error_stderr(&format!(
        "{DEFAULT} {CELL} fn main() {{ let x = new Cell<i64>(1).choose(new Cell<String>(\"x\")); }}"
    ));
    assert!(errors.contains("E0704"), "{errors}");
}

#[test]
fn wrong_conformance_rejected() {
    let errors = compile_error_stderr(
        "interface P { fn me(self) -> Self; } class Other { } class Cell<T> implements P { pub value: T; pub fn me(self) -> Other { return new Other(); } } fn main() { let c = new Cell<i64>(1); }",
    );
    assert!(errors.contains("E0417"), "{errors}");
}

#[test]
fn non_generic_receiver_unchanged() {
    one(
        &format!(
            "{DEFAULT} class Cell implements P {{ pub value: i64; }} fn main() {{ println(new Cell(15).me().value); }}"
        ),
        "15\n",
    );
}

#[test]
fn example_program() {
    one(
        include_str!("../../example/generic_class_self.wi"),
        "5\nsecond\n",
    );
}

#[test]
fn receiver_codegen_scales_with_instances_not_calls() {
    for instances in [1, 4, 16] {
        for calls in [1, 8, 32] {
            let mut source = String::from(
                "interface Identity { fn me(self) -> Self { return self; } } class Cell<T> implements Identity { pub value: T; static fn tag() -> i64 { return 1; } pub fn code(self) -> i64 { return Cell::tag(); } }",
            );
            for i in 0..instances {
                source.push_str(&format!("class Payload{i} {{ pub value: i64; }}"));
            }
            source.push_str("fn main() {");
            for i in 0..instances {
                source.push_str(&format!(
                    "let c{i} = new Cell<Payload{i}>(new Payload{i}({i}));"
                ));
                for _ in 0..calls {
                    source.push_str(&format!(
                        "println(c{i}.me().value.value); println(c{i}.code());"
                    ));
                }
            }
            source.push('}');
            let symbols = compile_and_collect_defined_symbols(&source, &[]);
            let receivers: Vec<_> = symbols
                .iter()
                .filter(|s| s.contains("Cell$mono$") && s.ends_with(".me"))
                .collect();
            assert_eq!(receivers.len(), instances, "{symbols:?}");
            let statics = symbols
                .iter()
                .filter(|s| s.contains("Cell$mono$") && s.ends_with(".tag"))
                .count();
            assert_eq!(statics, instances, "{symbols:?}");
            println!(
                "receiver_codegen instances={instances} calls_per_instance={calls} methods={}",
                receivers.len()
            );
        }
    }
}

#[test]
fn imported_receiver_with_same_named_consumer_argument() {
    runs(
        &[
            (
                "p.wi",
                "module p; interface P { fn me(self) -> Self { return self; } } pub class Cell<T> implements P { pub value: T; }",
            ),
            (
                "main.wi",
                "import p; class Cell { pub value: i64; } fn main() { println(new p::Cell<Cell>(new Cell(16)).me().value.value); }",
            ),
        ],
        "16\n",
    );
}

#[test]
fn private_imported_class_stays_private() {
    let errors = compile_temp_project_error_stderr(
        &[
            ("p.wi", "module p; class Cell<T> { pub value: T; }"),
            (
                "main.wi",
                "import p; fn main() { let c = new p::Cell<i64>(1); }",
            ),
        ],
        "main.wi",
    );
    assert!(errors.contains("private"), "{errors}");
}

#[test]
fn async_receiver_return() {
    one(
        "class Cell<T> { pub value: i64; pub async fn me(self) -> Cell<T> { return self; } } async fn main() { let c = await new Cell<i64>(17).me(); println(c.value); }",
        "17\n",
    );
}

#[test]
fn imported_explicit_receiver_return() {
    runs(
        &[
            (
                "p.wi",
                "module p; pub class Cell<T> { pub value: T; pub fn me(self) -> Cell<T> { return self; } }",
            ),
            (
                "main.wi",
                "import p; fn main() { println(new p::Cell<i64>(18).me().value); }",
            ),
        ],
        "18\n",
    );
}

#[test]
fn async_receiver_preserves_send_validation() {
    let errors = compile_error_stderr(
        "class Cell<T> { pub value: T; pub async fn me(self) -> Cell<T> { return self; } } async fn main() { let c = await new Cell<i64>(17).me(); }",
    );
    assert!(errors.contains("E2402"), "{errors}");
    assert!(!errors.contains("E0704"), "{errors}");
}

// Review regressions: access ownership must use the same specialization as self.
#[test]
fn access_control_internal_fields_and_calls() {
    for visibility in ["", "prot "] {
        one(
            &format!(
                "class Cell<T> {{ {visibility}value: T; pub init(self, value: T) {{ self.value = value; }} {visibility}fn read(self) -> T {{ return self.value; }} pub fn get(self) -> T {{ return self.read(); }} pub fn set(self, value: T) {{ self.value = value; }} }} fn main() {{ let c = new Cell<i64>(1); println(c.get()); c.set(2); println(c.get()); }}"
            ),
            "1\n2\n",
        );
    }
}

#[test]
fn access_control_external_fields_and_calls_rejected() {
    for visibility in ["", "prot "] {
        for access in ["println(c.value);", "c.value = 2;", "println(c.read());"] {
            let errors = compile_error_stderr(&format!(
                "class Cell<T> {{ {visibility}value: T; pub init(self, value: T) {{ self.value = value; }} {visibility}fn read(self) -> T {{ return self.value; }} }} fn main() {{ let c = new Cell<i64>(1); {access} }}"
            ));
            let code = if visibility.is_empty() {
                "E0501"
            } else {
                "E0503"
            };
            assert!(errors.contains(code), "{errors}");
        }
    }
}

#[test]
fn access_control_protected_base_and_super() {
    one(
        "open class Base { prot value: i64; pub init(self, value: i64) { self.value = value; } prot fn read(self) -> i64 { return self.value; } } class Cell<T> extends Base { extra: T; pub init(self, value: i64, extra: T) { super.init(value); self.extra = extra; } pub fn get(self) -> i64 { self.value = self.value + 1; return super.read(); } } fn main() { println(new Cell<i64>(3, 4).get()); }",
        "4\n",
    );
}

#[test]
fn access_control_imported_generic_class() {
    runs(
        &[
            (
                "p.wi",
                "module p; pub class Cell<T> { value: T; pub init(self, value: T) { self.value = value; } fn read(self) -> T { return self.value; } pub fn get(self) -> T { return self.read(); } }",
            ),
            (
                "main.wi",
                "import p; fn main() { println(new p::Cell<i64>(5).get()); }",
            ),
        ],
        "5\n",
    );
}

#[test]
fn static_access_internal() {
    for visibility in ["", "prot "] {
        one(
            &format!(
                "class Cell<T> {{ {visibility}static mut n: i64 = 1; pub value: T; {visibility}static fn read() -> i64 {{ return Cell::n; }} pub init(self, value: T) {{ self.value = value; Cell::n = 2; println(Cell::read()); }} pub fn get(self) -> i64 {{ Cell::n = 3; return Cell::read(); }} }} fn main() {{ let c = new Cell<i64>(0); println(c.get()); }}"
            ),
            "2\n3\n",
        );
    }
}

#[test]
fn static_access_unused_template() {
    one(
        "class Cell<T> { static n: i64 = 1; pub fn get(self) -> i64 { return Cell::n; } } fn main() {}",
        "",
    );
}

#[test]
fn static_access_external_rejected() {
    for visibility in ["", "prot "] {
        for access in [
            "println(Cell::n);",
            "Cell::n = 2;",
            "println(Cell<i64>::read());",
        ] {
            // Explicit arguments are supported for static calls; bare generic
            // static fields outside their class remain unresolved (E0502).
            let errors = compile_error_stderr(&format!(
                "class Cell<T> {{ {visibility}static mut n: i64 = 1; {visibility}static fn read() -> i64 {{ return Cell::n; }} }} fn main() {{ {access} }}"
            ));
            let code = if !access.contains("read") {
                "E0502"
            } else if visibility.is_empty() {
                "E0501"
            } else {
                "E0503"
            };
            assert!(errors.contains(code), "{errors}");
        }
        for access in [
            "println(Other::n);",
            "Other::n = 2;",
            "println(Other::read());",
        ] {
            let errors = compile_error_stderr(&format!(
                "class Other {{ {visibility}static mut n: i64 = 1; {visibility}static fn read() -> i64 {{ return Other::n; }} }} class Cell<T> {{ pub fn get(self) {{ {access} }} }} fn main() {{}}"
            ));
            let code = if access.contains(" = ") {
                // Static writes use E0419 for either visibility.
                "E0419"
            } else if !visibility.is_empty() {
                "E0503"
            } else if access.contains("read") {
                "E0501"
            } else {
                "E0419"
            };
            assert!(errors.contains(code), "{errors}");
        }
    }
}

#[test]
fn static_access_unrelated_class_rejected() {
    for visibility in ["", "prot "] {
        let errors = compile_error_stderr(&format!(
            "class Other {{ {visibility}static n: i64 = 1; }} class Cell<T> {{ pub fn get(self) -> i64 {{ return Other::n; }} }} fn main() {{}}"
        ));
        let code = if visibility.is_empty() {
            "E0419"
        } else {
            "E0503"
        };
        assert!(errors.contains(code), "{errors}");
    }
}

#[test]
fn static_access_imported_class() {
    runs(
        &[
            (
                "p.wi",
                "module p; pub class Cell<T> { static n: i64 = 7; pub value: T; static fn read() -> i64 { return Cell::n; } pub init(self, value: T) { self.value = value; println(Cell::n); } pub fn get(self) -> i64 { return Cell::read(); } }",
            ),
            (
                "main.wi",
                "import p; fn main() { println(new p::Cell<i64>(0).get()); }",
            ),
        ],
        "7\n7\n",
    );
}

#[test]
fn static_access_self_spelling() {
    one(
        "class Cell<T> { static n: i64 = 8; pub value: T; static fn read() -> i64 { return Self::n; } pub fn get(self) -> i64 { return Self::read(); } } fn main() { println(new Cell<i64>(0).get()); }",
        "8\n",
    );
}

#[test]
fn static_access_distinct_instances() {
    one(
        "class Cell<T> { static mut n: i64 = 0; pub value: T; pub fn bump(self) -> i64 { Cell::n = Cell::n + 1; return Cell::n; } } fn main() { let a = new Cell<i64>(0); let b = new Cell<bool>(true); println(a.bump()); println(a.bump()); println(b.bump()); }",
        "1\n2\n1\n",
    );
}
