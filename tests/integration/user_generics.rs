//! Concrete user generics: runtime behavior, diagnostics, modules, GC and reuse.
use super::support::*;

#[track_caller]
fn runs(source: &str, expected: &str) {
    let (output, ok) = compile_and_run(source);
    assert!(ok, "{output}");
    assert_eq!(output, expected);
}

#[track_caller]
fn rejects(source: &str, relevant_name: &str) {
    let stderr = compile_error_stderr(source);
    assert!(stderr.contains("error["), "{stderr}");
    assert!(stderr.contains(relevant_name), "{stderr}");
    assert!(!stderr.contains("internal compiler error"), "{stderr}");
    assert!(!stderr.contains("panicked at"), "{stderr}");
}

const BOX: &str = "class Box<T> { pub value: T; pub fn get(self) -> T { return self.value; } }";

#[test]
fn p01_example_runs() {
    runs(
        include_str!("../../example/user_generics.wi"),
        "42\nhello\n7\nwillow\n20\n",
    );
}

#[test]
fn p02_inferred_function_scalar_and_string() {
    runs(
        "fn identity<T>(x: T) -> T { return x; } fn main() { println(identity(42)); println(identity(\"text\")); println(identity(true)); }",
        "42\ntext\ntrue\n",
    );
}

#[test]
fn p03_explicit_multiple_function_parameters() {
    runs(
        "fn second<T, U>(a: T, b: U) -> U { return b; } fn main() { println(second<i64, String>(3, \"second\")); }",
        "second\n",
    );
}

#[test]
fn p04_inferred_multiple_function_parameters() {
    runs(
        "fn second<T, U>(a: T, b: U) -> U { return b; } fn main() { println(second(false, 71)); }",
        "71\n",
    );
}

#[test]
fn p05_repeated_consistent_parameter() {
    runs(
        "fn first<T>(a: T, b: T) -> T { return a; } fn main() { println(first(12, 19)); }",
        "12\n",
    );
}

#[test]
fn p06_repeated_parameter_conflict() {
    rejects(
        "fn first<T>(a: T, b: T) -> T { return a; } fn main() { first(1, \"bad\"); }",
        "T",
    );
}

#[test]
fn p07_recursive_function_reuses_concrete_instance() {
    runs(
        "fn descend<T>(x: T, n: i64) -> T { if n == 0 { return x; } return descend(x, n - 1); } fn main() { println(descend(\"recursive\", 20)); println(descend(9, 20)); }",
        "recursive\n9\n",
    );
}

#[test]
fn p08_class_scalar_field_and_method() {
    runs(
        &format!(
            "{BOX} fn main() {{ let b = new Box<i64>(31); println(b.value); println(b.get()); }}"
        ),
        "31\n31\n",
    );
}

#[test]
fn p09_class_string_field_and_method() {
    runs(
        &format!("{BOX} fn main() {{ let b = new Box<String>(\"stored\"); println(b.get()); }}"),
        "stored\n",
    );
}

#[test]
fn p10_class_object_payload() {
    runs(
        &format!(
            "{BOX} class Item {{ pub value: i64; }} fn main() {{ let b = new Box<Item>(new Item(17)); println(b.get().value); }}"
        ),
        "17\n",
    );
}

#[test]
fn p11_class_object_literal() {
    runs(
        &format!(
            "{BOX} fn main() {{ let b = Box<String> {{ value: \"literal\" }}; println(b.get()); }}"
        ),
        "literal\n",
    );
}

#[test]
fn p12_generic_class_parameter_inference() {
    runs(
        &format!(
            "{BOX} fn unwrap<T>(b: Box<T>) -> T {{ return b.get(); }} fn main() {{ println(unwrap(new Box<i64>(52))); println(unwrap(new Box<String>(\"unwrapped\"))); }}"
        ),
        "52\nunwrapped\n",
    );
}

#[test]
fn p13_nested_enum_payload_inference() {
    runs(
        "import std::collections::Array; enum Packet<T> { Items(Array<T>), Empty } fn main() { let p = Packet::Items([4, 8]); match p { Packet::Items(items) => println(items[1]), Packet::Empty => println(0), } }",
        "8\n",
    );
}

#[test]
fn p14_repeated_enum_payload_conflict() {
    rejects(
        "enum Pair<T> { Both(T, T) } fn main() { let p = Pair::Both(1, \"wrong\"); }",
        "T",
    );
}

#[test]
fn p15_explicit_function_wrong_type_arity() {
    rejects(
        "fn id<T>(x: T) -> T { return x; } fn main() { id<i64, String>(1); }",
        "type argument",
    );
}

#[test]
fn p16_explicit_unknown_type() {
    rejects(
        "fn id<T>(x: T) -> T { return x; } fn main() { id<Missing>(1); }",
        "Missing",
    );
}

#[test]
fn p17_duplicate_function_type_parameter() {
    rejects(
        "fn id<T, T>(x: T) -> T { return x; } fn main() { id(1); }",
        "T",
    );
}

#[test]
fn p18_unconstrained_function_parameter() {
    rejects(
        "fn keep<T, U>(x: T) -> T { return x; } fn main() { keep(1); }",
        "U",
    );
}

#[test]
fn p19_local_callable_shadows_generic_function() {
    runs(
        "fn pick<T>(x: T) -> T { return x; } fn main() { let pick = |x: i64| x + 10; println(pick(2)); }",
        "12\n",
    );
}

#[test]
fn p20_bare_generic_class_rejected() {
    rejects(&format!("{BOX} fn take(b: Box) {{}} fn main() {{}}"), "Box");
}

#[test]
fn p21_void_type_argument_rejected() {
    rejects(
        "fn keep<T>(n: i64) -> i64 { return n; } fn main() { keep<void>(1); }",
        "type argument",
    );
}

#[test]
fn p22_explicit_argument_type_conflict() {
    rejects(
        "fn id<T>(x: T) -> T { return x; } fn main() { id<i64>(\"wrong\"); }",
        "error[",
    );
}

const MODULE: &str = "module util; pub fn id<T>(x: T) -> T { return x; } pub class Box<T> { pub value: T; pub fn get(self) -> T { return self.value; } }";

#[track_caller]
fn module_runs(entry: &str, expected: &str) {
    let project = TestProject::new("user_generics", &[("main.wi", entry), ("util.wi", MODULE)]);
    let compilation = project.compile("main.wi");
    assert!(
        compilation.status.success(),
        "{}",
        String::from_utf8_lossy(&compilation.stderr)
    );
    let execution = project.run();
    assert!(
        execution.status.success(),
        "{}",
        String::from_utf8_lossy(&execution.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&execution.stdout), expected);
}

#[test]
fn p23_qualified_generic_function() {
    module_runs(
        "import util; fn main() { println(util::id(61)); println(util::id<String>(\"qualified\")); }",
        "61\nqualified\n",
    );
}

#[test]
fn p24_imported_generic_function_alias() {
    module_runs(
        "import util::id as identity; fn main() { println(identity(73)); }",
        "73\n",
    );
}

#[test]
fn p25_qualified_generic_class() {
    module_runs(
        "import util; fn main() { let b = new util::Box<String>(\"module\"); println(b.get()); }",
        "module\n",
    );
}

#[test]
fn p26_imported_generic_class_alias() {
    module_runs(
        "import util::Box as Holder; fn main() { let b = new Holder<i64>(81); println(b.get()); }",
        "81\n",
    );
}

#[test]
fn p27_generic_payload_survives_gc_stress() {
    let source = format!(
        "{BOX} class Item {{ pub text: String; }} fn id<T>(x: T) -> T {{ return x; }} fn main() {{ let b = new Box<Item>(new Item(\"alive\")); let text = new Box<String>(\"rooted\"); let extra = new Box<String>(\"allocation\"); println(id(b).get().text); println(text.get()); println(extra.get()); }}"
    );
    let (output, ok) = compile_and_run_gc_stress(&source);
    assert!(ok, "{output}");
    assert_eq!(output, "alive\nrooted\nallocation\n");
}

#[test]
fn p28_repeated_calls_emit_one_instance_per_type() {
    let symbols = compile_and_collect_defined_symbols(
        "fn descend<T>(x: T, n: i64) -> T { if n == 0 { return x; } return descend(x, n - 1); } fn main() { println(descend(1, 4)); println(descend(2, 4)); println(descend(\"x\", 4)); println(descend(\"y\", 4)); }",
        &[],
    );
    let instances: Vec<_> = symbols
        .iter()
        .filter(|name| name.starts_with("descend$mono$"))
        .collect();
    assert_eq!(instances.len(), 2, "{symbols:#?}");
}

#[test]
fn p29_class_wrong_type_arity_rejected() {
    rejects(
        &format!("{BOX} fn main() {{ let b = new Box<i64, String>(1); }}"),
        "Box",
    );
}

#[test]
fn p30_class_void_type_argument_rejected() {
    rejects(
        "class Marker<T> { pub value: i64; } fn main() { let x = new Marker<void>(1); }",
        "type argument",
    );
}

#[test]
fn p31_nested_function_array_inference() {
    runs(
        "import std::collections::Array; fn first<T>(items: Array<T>) -> T { return items[0]; } fn main() { println(first([3, 5])); println(first([\"nested\"])); }",
        "3\nnested\n",
    );
}

#[test]
fn p32_instantiated_body_is_type_checked() {
    rejects(
        "fn add<T>(x: T) -> T { return x + 1; } fn main() { add(true); }",
        "error[",
    );
}

#[test]
fn p33_explicit_constructor_and_mutating_method() {
    runs(
        "class Cell<T> { pub value: T; pub init(self, value: T) { self.value = value; } pub fn set(self, value: T) { self.value = value; } } fn main() { let c = new Cell<i64>(1); c.set(22); println(c.value); }",
        "22\n",
    );
}

#[test]
fn p34_unused_generic_body_rejects_unconstrained_arithmetic() {
    rejects(
        "fn invalid<T>(x: T) -> T { return x + 1; } fn main() {}",
        "T",
    );
}

#[test]
fn p35_type_parameter_cannot_shadow_concrete_type() {
    rejects(
        "class Payload { pub value: i64; } fn identity<Payload>(x: Payload) -> Payload { return x; } fn main() {}",
        "Payload",
    );
}

#[test]
fn p36_uninstantiated_generic_function_value_is_rejected() {
    rejects(
        "fn identity<T>(x: T) -> T { return x; } fn main() { let f = identity; }",
        "identity",
    );
}

#[track_caller]
fn compile_expansion_promptly(source: &str) -> (std::process::Output, String) {
    // Bound the compiler itself: this regression used to grow the instance
    // queue forever, so a diagnostic-only helper could hang the entire suite.
    // Provision the runtime outside the compiler deadline so concurrent Cargo
    // work cannot turn an artifact-lock wait into an expansion timeout.
    let runtime = build_runtime_staticlib(false);
    let id = unique_test_id();
    let source_path = temp_path(format!("generic_growth_{id}.wi"));
    let binary_path = temp_path(format!("generic_growth_{id}"));
    std::fs::write(&source_path, source).unwrap();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["build", &source_path, "-o", &binary_path])
        .arg("--runtime-lib")
        .arg(&*runtime)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (output, timed_out) = wait_promptly(child);
    let _ = std::fs::remove_file(source_path);
    if timed_out {
        remove_output_artifacts(&binary_path);
    }
    assert!(
        !timed_out,
        "generic compilation did not terminate within 20 seconds"
    );
    (output, binary_path)
}

fn wait_promptly(mut child: std::process::Child) -> (std::process::Output, bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut timed_out = false;
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            timed_out = true;
            child.kill().unwrap();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    (output, timed_out)
}

#[track_caller]
fn rejects_expansion_promptly(source: &str, expected_fragments: &[&str]) {
    let (output, binary_path) = compile_expansion_promptly(source);
    remove_output_artifacts(&binary_path);
    assert!(
        !output.status.success(),
        "growing generic expansion must be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error["), "{stderr}");
    assert!(
        expected_fragments
            .iter()
            .any(|fragment| stderr.contains(fragment)),
        "{stderr}"
    );
    assert!(!stderr.contains("internal compiler error"), "{stderr}");
}

#[test]
fn p37_recursive_growing_class_reports_non_finite_expansion() {
    rejects_expansion_promptly(
        "import std::collections::Array; class Nest<T> { pub next: Nest<Array<T>>; } fn take(x: Nest<i64>) {} fn main() {}",
        &["recursive"],
    );
}

#[test]
fn p38_unused_generic_preserves_interface_default_methods() {
    runs(
        "fn identity<T>(x: T) -> T { return x; } interface Answer { fn answer(self) -> i64 { return 42; } } class Value implements Answer {} fn main() { println(new Value().answer()); }",
        "42\n",
    );
}

#[test]
fn p39_explicit_function_arguments_contextually_type_lambda() {
    runs(
        "fn apply<T, U>(x: T, f: fn(T) -> U) -> U { return f(x); } fn main() { println(apply<i64, i64>(4, |x| x + 6)); }",
        "10\n",
    );
}

#[track_caller]
fn custom_module_runs(module: &str, entry: &str, expected: &str) {
    let project = TestProject::new(
        "generic_module_scope",
        &[("util.wi", module), ("main.wi", entry)],
    );
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
fn p40_imported_generic_retains_private_helper_scope() {
    custom_module_runs(
        "module util; fn marker() -> i64 { return 19; } pub fn touch<T>(x: T) -> T { println(marker()); return x; }",
        "import util; fn main() { println(util::touch(\"kept\")); }",
        "19\nkept\n",
    );
}

#[test]
fn p41_imported_generic_accepts_module_local_enum() {
    custom_module_runs(
        "module util; pub enum Color { Red, Blue } pub fn choose<T>(color: Color, x: T) -> T { return x; }",
        "import util; fn main() { println(util::choose(util::Color::Red, 28)); }",
        "28\n",
    );
}

#[test]
fn p42_imported_generic_accepts_module_local_class() {
    custom_module_runs(
        "module util; pub class Data { pub value: i64; } pub fn choose<T>(data: Data, x: T) -> T { println(data.value); return x; }",
        "import util; fn main() { println(util::choose(new util::Data(18), \"scoped\")); }",
        "18\nscoped\n",
    );
}

#[test]
fn p43_imported_generic_accepts_caller_local_class_argument() {
    module_runs(
        "import util; class Local { pub value: i64; } fn main() { println(util::id<Local>(new Local(67)).value); let b = new util::Box<Local>(new Local(68)); println(b.get().value); }",
        "67\n68\n",
    );
}

#[test]
fn p44_import_aliases_share_one_specialization() {
    let project = TestProject::new(
        "generic_alias_reuse",
        &[
            (
                "util.wi",
                "module util; pub fn descend<T>(x: T, n: i64) -> T { if n == 0 { return x; } return descend(x, n - 1); }",
            ),
            (
                "main.wi",
                "import util::descend as first; import util::descend as second; fn main() { println(first(1, 3)); println(second(2, 3)); }",
            ),
        ],
    );
    let compiled = project.compile_with_env("main.wi", &[("WILLOW_KEEP_OBJECT", "1")]);
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let symbols = project.defined_object_symbols();
    let instances: Vec<_> = symbols
        .iter()
        .filter(|name| name.contains("descend$mono$"))
        .collect();
    assert_eq!(instances.len(), 1, "{symbols:#?}");
    let output = project.run();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "1\n2\n");
}

#[test]
fn p76_generic_classes_preserve_local_interface_default_body_identity() {
    runs(
        "interface Answer { fn answer(self) -> i64 { return 76; } } class Box<T> implements Answer { pub value: T; } fn read(answer: Answer) -> i64 { return answer.answer(); } fn main() { let number = new Box<i64>(1); let text = new Box<String>(\"stored\"); println(number.answer()); println(text.answer()); println(read(text)); }",
        "76\n76\n76\n",
    );
}

#[test]
#[ignore = "willow-rvpp: cross-module default bodies cannot reach provider items"]
fn p77_generic_classes_preserve_imported_default_private_helper_scope() {
    custom_module_runs(
        "module util; fn helper() -> i64 { return 77; } pub interface Answer { fn answer(self) -> i64 { return helper(); } }",
        "import util; class Box<T> implements util::Answer { pub value: T; } fn read(answer: util::Answer) -> i64 { return answer.answer(); } fn main() { println(new Box<i64>(1).answer()); println(read(new Box<String>(\"stored\"))); }",
        "77\n77\n",
    );
}

#[test]
#[ignore = "willow-rvpp: cross-module default bodies cannot reach provider items"]
fn p78_generic_classes_preserve_imported_default_generic_helper_calls() {
    custom_module_runs(
        "module util; fn identity<T>(x: T) -> T { return x; } pub interface Answer { fn answer(self) -> i64 { return identity(78); } }",
        "import util; class Box<T> implements util::Answer { pub value: T; } fn read(answer: util::Answer) -> i64 { return answer.answer(); } fn main() { println(new Box<i64>(1).answer()); println(read(new Box<String>(\"stored\"))); }",
        "78\n78\n",
    );
}

#[test]
fn p45_nested_array_and_option_class_payloads_survive_gc_stress() {
    let source = format!(
        "import std::collections::Array; {BOX} fn main() {{ let values: Array<Box<String>> = [new Box<String>(\"array\")]; let maybe: Option<Box<String>> = Some(new Box<String>(\"option\")); let extra = new Box<String>(\"allocate\"); println(values[0].get()); println(maybe.unwrap().get()); println(extra.get()); }}"
    );
    let (output, ok) = compile_and_run_gc_stress(&source);
    assert!(ok, "{output}");
    assert_eq!(output, "array\noption\nallocate\n");
}

#[test]
fn p46_specialization_symbols_are_deterministic_across_builds() {
    let source = "fn descend<T>(x: T, n: i64) -> T { if n == 0 { return x; } return descend(x, n - 1); } fn main() { println(descend(1, 2)); println(descend(\"a\", 2)); }";
    let first: Vec<_> = compile_and_collect_defined_symbols(source, &[])
        .into_iter()
        .filter(|name| name.starts_with("descend$mono$"))
        .collect();
    let second: Vec<_> = compile_and_collect_defined_symbols(source, &[])
        .into_iter()
        .filter(|name| name.starts_with("descend$mono$"))
        .collect();
    assert_eq!(first.len(), 2);
    assert_eq!(first, second);
}

#[test]
fn p47_duplicate_enum_type_parameters_rejected() {
    rejects("enum Pair<T, T> { Value(T) } fn main() {}", "T");
}

#[test]
fn p48_unknown_explicit_enum_type_argument_rejected() {
    rejects(
        "enum Value<T> { Item(T) } fn main() { let x = Value<Missing>::Item(1); }",
        "Missing",
    );
}

#[test]
fn p49_repeated_call_growth_keeps_one_generated_instance() {
    for count in [1, 16, 256] {
        let mut source = String::from("fn id<T>(x: T) -> T { return x; } fn main() {");
        for _ in 0..count {
            source.push_str("id(1);");
        }
        source.push('}');
        let symbols = compile_and_collect_defined_symbols(&source, &[]);
        let instances = symbols.iter().filter(|s| s.starts_with("id$mono$")).count();
        assert_eq!(instances, 1, "calls={count}, symbols={symbols:?}");
    }
}

#[test]
fn p50_imported_generics_preserve_caller_enum_identity_and_payloads() {
    custom_module_runs(
        MODULE,
        r#"
import util;
enum Local { Text(String), Count(i64) }
fn main() {
    let direct = util::id<Local>(Local::Text("enum"));
    match direct {
        Local::Text(text) => println(text),
        Local::Count(number) => println(number),
    }
    let boxed = new util::Box<Local>(Local::Count(45));
    match boxed.get() {
        Local::Text(text) => println(text),
        Local::Count(number) => println(number),
    }
}
"#,
        "enum\n45\n",
    );
}

#[test]
fn p51_imported_generics_do_not_rebind_caller_class_to_provider_namesake() {
    custom_module_runs(
        &format!("{MODULE} pub class Local {{ pub provider_only: bool; }}"),
        r#"
import util;
class Local { pub text: String; pub count: i64; }
fn main() {
    let direct = util::id<Local>(new Local("caller", 17));
    println(direct.text);
    println(direct.count);
    let boxed = new util::Box<Local>(new Local("boxed", 23));
    println(boxed.get().text);
    println(boxed.get().count);
}
"#,
        "caller\n17\nboxed\n23\n",
    );
}

#[test]
fn p52_imported_generic_nested_caller_payloads_survive_gc_stress() {
    let project = TestProject::new(
        "generic_caller_nested_gc",
        &[
            ("util.wi", MODULE),
            (
                "main.wi",
                r#"
import util;
class Leaf { pub text: String; }
class Branch { pub leaf: Leaf; pub other: Option<Leaf>; }
fn main() {
    let boxed = new util::Box<Branch>(new Branch(new Leaf("leaf"), Some(new Leaf("option"))));
    let extra = new util::Box<String>("allocated");
    let branch = util::id(boxed).get();
    println(branch.leaf.text);
    println(branch.other.unwrap().text);
    println(extra.get());
}

"#,
            ),
        ],
    );
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let output = project.run_with_env(&[("WILLOW_GC_STRESS", "alloc")]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "leaf\noption\nallocated\n"
    );
}

#[track_caller]
fn consumer_local_identities_remain_distinct(aliases: bool) {
    let (imports, identity, holder) = if aliases {
        (
            "import util::id as identity; import util::Box as Holder;",
            "identity",
            "Holder",
        )
    } else {
        ("import util;", "util::id", "util::Box")
    };
    let first = format!(
        "module a; {imports} class Local {{ pub text: String; }} pub fn run() {{ let direct = {identity}<Local>(new Local(\"a-id\")); println(direct.text); let boxed = new {holder}<Local>(new Local(\"a-box\")); println(boxed.get().text); }}"
    );
    let second = format!(
        "module b; {imports} class Local {{ pub leading: i64; pub label: String; pub trailing: bool; }} pub fn run() {{ let direct = {identity}<Local>(new Local(101, \"b-id\", true)); println(direct.leading); println(direct.label); println(direct.trailing); let boxed = new {holder}<Local>(new Local(202, \"b-box\", false)); println(boxed.get().leading); println(boxed.get().label); println(boxed.get().trailing); }}"
    );
    let project = TestProject::new(
        "generic_consumer_identities",
        &[
            ("util.wi", MODULE),
            ("a.wi", &first),
            ("b.wi", &second),
            (
                "main.wi",
                "import a; import b; fn main() { a::run(); b::run(); }",
            ),
        ],
    );
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let output = project.run_with_env(&[("WILLOW_GC_STRESS", "alloc")]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "a-id\na-box\n101\nb-id\ntrue\n202\nb-box\nfalse\n"
    );
}

#[test]
fn p53_same_named_consumer_classes_have_distinct_build_wide_specializations() {
    consumer_local_identities_remain_distinct(false);
}

#[test]
fn p54_aliases_preserve_distinct_consumer_class_specializations() {
    consumer_local_identities_remain_distinct(true);
}

#[test]
fn p58_duplicate_generic_functions_are_rejected() {
    rejects(
        "fn id<T>(x: T) -> T { return x; } fn id<U>(x: U) -> U { return x; } fn main() {}",
        "already declared",
    );
}

#[test]
fn p59_duplicate_generic_classes_are_rejected() {
    rejects(
        "class Box<T> { pub value: T; } class Box<U> { pub other: U; } fn main() {}",
        "already declared",
    );
}

#[test]
fn p60_generic_and_concrete_function_collisions_are_order_independent() {
    for declarations in [
        "fn id<T>(x: T) -> T { return x; } fn id(x: i64) -> i64 { return x; }",
        "fn id(x: i64) -> i64 { return x; } fn id<T>(x: T) -> T { return x; }",
        "const id: i64 = 1; fn id<T>(x: T) -> T { return x; }",
        "fn id<T>(x: T) -> T { return x; } const id: i64 = 1;",
    ] {
        rejects(
            &format!("{declarations} fn main() {{}}"),
            "already declared",
        );
    }
}

#[test]
fn p61_generic_and_concrete_class_collisions_are_order_independent() {
    for declarations in [
        "class Box<T> { pub value: T; } class Box { pub value: i64; }",
        "class Box { pub value: i64; } class Box<T> { pub value: T; }",
        "class Box<T> { pub value: T; } fn Box() {}",
    ] {
        rejects(
            &format!("{declarations} fn main() {{}}"),
            "already declared",
        );
    }
}

#[test]
fn p62_reserved_names_cannot_be_generic_templates() {
    rejects(
        "class PanicInfo<T> { pub value: T; } fn main() {}",
        "reserved runtime type",
    );
    rejects(
        "fn recover<T>(x: T) -> T { return x; } fn main() {}",
        "reserved builtin function",
    );
}

#[test]
fn p63_generic_declarations_cannot_shadow_imported_names() {
    for entry in [
        "import util::id; fn id<T>(x: T) -> T { return x; } fn main() {}",
        "import util::Box; class Box<T> { pub value: T; } fn main() {}",
    ] {
        let stderr = compile_temp_project_error_stderr(
            &[("util.wi", MODULE), ("main.wi", entry)],
            "main.wi",
        );
        assert!(
            stderr.contains("defined both by an import and a local declaration"),
            "{stderr}"
        );
        assert!(!stderr.contains("internal compiler error"), "{stderr}");
    }
}

#[test]
fn p64_function_values_reject_explicit_type_arguments() {
    rejects(
        "fn main() { let f = |x: i64| x; f<String>(1); }",
        "does not accept type arguments",
    );
    rejects(
        "fn pick<T>(x: T) -> T { return x; } fn main() { let pick = |x: i64| x; pick<i64>(1); }",
        "does not accept type arguments",
    );
}

#[test]
fn p65_non_generic_class_constructor_rejects_type_arguments() {
    rejects(
        "class C { pub value: i64; } fn main() { let c = new C<i64>(1); }",
        "does not accept type arguments",
    );
}

#[test]
fn p66_non_generic_class_literal_rejects_type_arguments() {
    rejects(
        "class C { pub value: i64; } fn main() { let c = C<i64> { value: 1 }; }",
        "does not accept type arguments",
    );
}

#[test]
fn p67_unused_generic_body_checks_reference_argument_modes() {
    rejects(
        "fn id<T>(x: T) -> T { return x; } fn bad<T>(x: T) -> T { return id(&x); } fn main() {}",
        "reference",
    );
}

#[test]
fn p68_generic_mutable_reference_writes_back_scalar_value() {
    runs(
        "fn replace<T>(target: &mut T, value: T) { target = value; } fn main() { let mut value = 1; replace(&value, 29); println(value); }",
        "29\n",
    );
}

#[test]
fn p69_generic_mutable_reference_arguments_must_not_alias() {
    rejects(
        "fn copy<T>(target: &mut T, source: &mut T) { target = source; } fn main() { let mut value = 1; copy(&value, &value); }",
        "aliases a mutable reference",
    );
}

#[test]
fn p55_repeated_generic_parameter_unifies_nominal_import_aliases() {
    let project = TestProject::new(
        "generic_nominal_alias_constraints",
        &[
            (
                "shapes.wi",
                "module shapes; pub class Data { pub value: i64; }",
            ),
            (
                "util.wi",
                "module util; pub fn first<T>(a: T, b: T) -> T { return a; }",
            ),
            (
                "main.wi",
                "import shapes as left; import shapes as right; import shapes::Data as Item; import util; fn main() { println(util::first(new left::Data(10), new right::Data(20)).value); println(util::first<Item>(new right::Data(30), new left::Data(40)).value); }",
            ),
        ],
    );
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
    assert_eq!(String::from_utf8_lossy(&output.stdout), "10\n30\n");
}

#[test]
fn p56_finite_recursive_generic_class_specializations_survive_gc() {
    let source = "class Link<T> { pub next: Option<Link<i64>>; } fn main() { let string_link = new Link<String>(None); let integer_link = new Link<i64>(None); let rooted = new Link<String>(Some(integer_link)); let extra = new Link<i64>(None); println(string_link.next.is_none()); println(rooted.next.unwrap().next.is_none()); println(extra.next.is_none()); }";
    let (output, ok) = compile_and_run_gc_stress(source);
    assert!(ok, "{output}");
    assert_eq!(output, "true\ntrue\ntrue\n");
}

#[test]
fn p57_imported_generics_transport_caller_generic_enum_heads() {
    custom_module_runs(
        MODULE,
        r#"
import util;
enum Payload<U> { Value(U) }
fn main() {
    let direct = util::id<Payload<String>>(Payload<String>::Value("direct"));
    match direct { Payload::Value(text) => println(text), }
    let boxed = new util::Box<Payload<String>>(Payload<String>::Value("boxed"));
    match boxed.get() { Payload::Value(text) => println(text), }
}
"#,
        "direct\nboxed\n",
    );
}

#[test]
fn p70_inferred_generic_contextually_types_fixed_enum_argument() {
    runs(
        "fn choose<T>(flag: Option<i64>, x: T) -> T { return x; } fn main() { println(choose(None, 71)); }",
        "71\n",
    );
}

#[test]
fn p71_inferred_generic_preserves_fixed_parameter_class_upcast() {
    runs(
        "open class Base {} class Child extends Base {} fn choose<T>(base: Base, x: T) -> T { return x; } fn main() { println(choose(new Child(), 72)); }",
        "72\n",
    );
}

#[test]
fn p72_nested_generic_reference_indices_preserve_results() {
    runs(
        "import std::collections::Array; fn index<T>(x: &T) -> i64 { return 0; } fn main() { let a = [1]; println(index(&a[index(&a[index(&a[0])])])); }",
        "0\n",
    );
}

#[test]
fn p73_exponentially_growing_function_type_arguments_are_bounded() {
    rejects_expansion_promptly(
        "enum Pair<A, B> { Both(A, B) } fn grow<T>() { grow<Pair<T, T>>(); } fn main() { grow<i64>(); }",
        &["recurs", "resource"],
    );
}

#[test]
fn p74_generic_wrapper_preserves_imported_template_definition_scope() {
    custom_module_runs(
        "module util; pub enum Color { Red } fn helper(x: i64) -> i64 { return x; } pub fn choose<T>(color: Color, x: T) -> T { println(helper(74)); return x; }",
        "import util; fn relay<T>(x: T) -> T { return util::choose(util::Color::Red, x); } fn main() { println(relay(75)); }",
        "74\n75\n",
    );
}

#[test]
fn p79_explicit_generic_value_argument_preserves_class_upcasts() {
    runs(
        "open class Base { pub open fn value(self) -> i64 { return 1; } } class Child extends Base { pub override fn value(self) -> i64 { return 79; } } fn id<T>(x: T) -> T { return x; } fn main() { println(id<Base>(new Child()).value()); }",
        "79\n",
    );
}

#[test]
fn p80_explicit_generic_value_argument_preserves_interface_boxing() {
    runs(
        "interface Answer { fn value(self) -> i64; } class Concrete implements Answer { pub fn value(self) -> i64 { return 80; } } fn id<T>(x: T) -> T { return x; } fn main() { let answer = id<Answer>(new Concrete()); println(answer.value()); }",
        "80\n",
    );
}

#[test]
fn p81_explicit_generic_reference_argument_does_not_upcast_storage() {
    rejects(
        "open class Base {} class Child extends Base {} fn read<T>(x: &T) -> T { return x; } fn main() { let child = new Child(); read<Base>(&child); }",
        "conflicting types",
    );
}

#[test]
fn p82_explicit_generic_reference_argument_does_not_box_storage() {
    rejects(
        "interface Answer { fn value(self) -> i64; } class Concrete implements Answer { pub fn value(self) -> i64 { return 1; } } fn read<T>(x: &T) -> T { return x; } fn main() { let concrete = new Concrete(); read<Answer>(&concrete); }",
        "conflicting types",
    );
}

#[test]
fn p75_nonregular_recursive_enum_has_finite_value_layouts() {
    let source = r#"
import std::collections::Array;
enum Grow<T> { End, Next(Grow<Array<T>>) }
fn main() {
    let end = Grow<i64>::End();
    match end { Grow::End => println(1), Grow::Next(next) => println(0), }
    let nested = Grow<i64>::Next(Grow<Array<i64>>::End());
    match nested {
        Grow::End => println(0),
        Grow::Next(next) => {
            match next { Grow::End => println(2), Grow::Next(deeper) => println(0), }
        },
    }
}
"#;
    let (compiled, binary_path) = compile_expansion_promptly(source);
    if !compiled.status.success() {
        remove_output_artifacts(&binary_path);
        panic!("{}", String::from_utf8_lossy(&compiled.stderr));
    }
    let child = std::process::Command::new(&binary_path)
        .env("WILLOW_GC_STRESS", "alloc")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (output, timed_out) = wait_promptly(child);
    remove_output_artifacts(&binary_path);
    assert!(
        !timed_out,
        "finite recursive enum execution exceeded 20 seconds"
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "1\n2\n");
}

#[test]
#[ignore = "willow-rvpp: cross-module default bodies cannot reach provider items"]
fn p83_generic_class_default_preserves_provider_generic_allocation() {
    custom_module_runs(
        "module util; class Payload<T> { pub value: T; pub fn get(self) -> T { return self.value; } } pub interface Answer { fn answer(self) -> i64 { return new Payload<i64>(83).get(); } }",
        "import util; class Box<T> implements util::Answer { pub value: T; } fn main() { println(new Box<String>(\"stored\").answer()); }",
        "83\n",
    );
}

#[test]
fn p84_generic_classes_receive_imported_interface_defaults() {
    custom_module_runs(
        "module util; pub interface Answer { fn base(self) -> i64; fn answer(self) -> i64 { return self.base() + 1; } }",
        "import util; class Box<T> implements util::Answer { pub value: T; pub fn base(self) -> i64 { return 83; } } fn read(answer: util::Answer) -> i64 { return answer.answer(); } fn main() { println(new Box<i64>(1).answer()); println(read(new Box<String>(\"stored\"))); }",
        "84\n84\n",
    );
}

#[test]
fn p85_generic_class_imported_default_reports_provider_scope_like_nongeneric() {
    for class in [
        "class Box<T> implements util::Answer { pub value: T; }",
        "class Box implements util::Answer { pub value: i64; }",
    ] {
        let project = TestProject::new(
            "generic_module_scope",
            &[
                (
                    "util.wi",
                    "module util; fn helper() -> i64 { return 77; } pub interface Answer { fn answer(self) -> i64 { return helper(); } }",
                ),
                ("main.wi", &format!("import util; {class} fn main() {{ }}")),
            ],
        );
        let compiled = project.compile("main.wi");
        let stderr = String::from_utf8_lossy(&compiled.stderr);
        assert!(!compiled.status.success(), "{class}");
        assert!(
            stderr.contains("E0350") && !stderr.contains("internal compiler error"),
            "{class}: {stderr}"
        );
    }
}
