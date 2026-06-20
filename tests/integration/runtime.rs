use super::support::*;

#[test]
fn test_println_i64() {
    let (out, ok) = compile_and_run("fn main() { println(42); }");
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_println_string_literal() {
    let (out, ok) = compile_and_run(r#"fn main() { println("Hello, world!"); }"#);
    assert!(ok, "compilation failed");
    assert_eq!(out, "Hello, world!\n");
}

#[test]
fn test_print_string_variable() {
    let src = r#"
fn main() {
    let greeting: String = "hello";
    print(greeting);
    println(" willow");
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "hello willow\n");
}

#[test]
fn test_string_concatenation() {
    let src = r#"
fn greet(name: String) -> String {
    return "Hello, " + name;
}

fn main() {
    let punctuation = "!";
    println(greet("Willow") + punctuation);
    println("a" + "b" + "c");
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "Hello, Willow!\nabc\n");
}

#[test]
fn test_string_concatenation_rejects_non_string_rhs() {
    assert_compile_error_contains(
        r#"
fn main() {
    println("count: " + 3);
}
"#,
        &[
            "error[E0202]",
            "cannot concatenate `String` with `i64`",
            ".toString()",
        ],
    );
}

#[test]
fn test_print_no_newline() {
    let (out, ok) = compile_and_run("fn main() { print(1); print(2); println(3); }");
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "123");
}

#[test]
fn test_println_bool() {
    let (out, ok) = compile_and_run("fn main() { println(true); println(false); }");
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn test_println_f64() {
    let (out, ok) = compile_and_run("fn main() { println(2.5); println(-0.5); }");
    assert!(ok, "compilation failed");
    assert_eq!(out, "2.5\n-0.5\n");
}

#[test]
fn test_print_expression_results() {
    let src = r#"
fn main() {
    print(1 + 2);
    print(3 * 4);
    println(5 == 5);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "312true\n");
}

#[test]
fn test_comments_are_ignored() {
    let src = r#"
fn main() {
    // Comments can sit on their own line.
    println(1); // And after statements.
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1");
}

// ── Class codegen ────────────────────────────────────────────────────────────

#[test]
fn test_class_instantiation_and_field_access() {
    let src = r#"
class Point {
    pub init(self, x: i64, y: i64) {
        self.x = x;
        self.y = y;
    }
    x: i64;
    y: i64;

    pub fn get_x(self) -> i64 { return self.x; }
    pub fn get_y(self) -> i64 { return self.y; }
}

fn main() {
    let p = new Point(10, 20);
    println(p.get_x());
    println(p.get_y());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "10\n20\n");
}

#[test]
fn test_class_gc_ref_mask_rejects_gc_field_beyond_coverage() {
    let mut fields = String::new();
    let mut args = Vec::new();
    for i in 0..63 {
        fields.push_str(&format!("    pub n{i}: i64;\n"));
        args.push(i.to_string());
    }
    fields.push_str("    pub late: String;\n");
    args.push("\"late\"".to_string());

    let src = format!(
        r#"
class TooWide {{
{fields}}}

fn main() {{
    let value = new TooWide({});
    println(1);
}}
"#,
        args.join(", ")
    );
    assert_compile_error_contains(
        &src,
        &[
            "TooWide",
            "late",
            "outside gc_ref_mask coverage",
            "class_type_id",
        ],
    );
}

#[test]
fn test_class_method_with_arithmetic() {
    let src = r#"
class Counter {
    pub init(self, count: i64) {
        self.count = count;
    }
    count: i64;

    pub fn value(self) -> i64 { return self.count; }
    pub fn doubled(self) -> i64 { return self.count * 2; }
    pub fn add(self, n: i64) -> i64 { return self.count + n; }
}

fn main() {
    let c = new Counter(5);
    println(c.value());
    println(c.doubled());
    println(c.add(10));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "5\n10\n15\n");
}

#[test]
fn test_class_method_call_chained_in_println() {
    let src = r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}

fn main() {
    let b = new Box(99);
    println(b.get());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "99\n");
}

// ── GC ───────────────────────────────────────────────────────────────────────

#[test]
fn test_gc_allocated_bytes_increases_on_class_alloc() {
    let src = r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let before = gc_allocated_bytes();
    let b = new Box(42);
    let after = gc_allocated_bytes();
    println(b.get());
    println(after > before);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "42\ntrue\n");
}

#[test]
fn test_gc_collect_reclaims_unrooted_objects() {
    // alloc_node allocates a Node and returns its value field (i64, not a GC pointer).
    // When alloc_node returns, the Node's root is popped, so the Node has no live roots.
    // gc_collect() in main can then reclaim it, leaving gc_allocated_bytes() == 0.
    let src = r#"
class Node {
    pub init(self, value: i64) {
        self.value = value;
    }
    value: i64;
    pub fn get(self) -> i64 { return self.value; }
}
fn alloc_node() -> i64 {
    let n = new Node(7);
    return n.get();
}
fn main() {
    let v = alloc_node();
    println(v);
    gc_collect();
    println(gc_allocated_bytes());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "7\n0\n");
}

#[test]
fn test_gc_does_not_collect_live_rooted_objects() {
    // A rooted object (n is in scope when gc_collect() runs) must not be freed.
    let src = r#"
class Node {
    pub init(self, value: i64) {
        self.value = value;
    }
    value: i64;
    pub fn get(self) -> i64 { return self.value; }
}
fn main() {
    let n = new Node(42);
    gc_collect();
    println(n.get());
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "42\ntrue\n");
}

#[test]
fn test_gc_traces_nullable_reference_fields() {
    let src = r#"
class Node {
    pub value: i64;
    pub next: Node?;
}

fn make_pair() -> Node {
    let tail = new Node(2, nil);
    return new Node(1, tail);
}

fn main() {
    let head = make_pair();
    gc_collect();
    println(head.value);
    let next = head.next;
    if next != nil {
        println(next.value);
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "nullable reference field should keep child object alive"
    );
    assert_eq!(out, "1\n2\n");
}

#[test]
fn test_gc_ignores_nil_nullable_reference_fields() {
    let src = r#"
class Node {
    pub init(self, value: i64, next: Node?) {
        self.value = value;
        self.next = next;
    }
    pub value: i64;
    next: Node?;
}

fn main() {
    let head = new Node(1, nil);
    gc_collect();
    println(head.value);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "nil nullable field should be ignored safely by GC");
    assert_eq!(out, "1\ntrue\n");
}

// ── Example files ───────────────────────────────────────────────────────────

#[test]
fn test_runnable_example_files_compile_and_run() {
    let cases = [
        ("example/arithmetic.wi", "27\n15\n126\n3\n3\n54\n3\ntrue\n"),
        ("example/array_growth.wi", "5\n55\n25\n16\n3\n"),
        ("example/arrays.wi", "4\n10\n40\n100\n99\n2\nbob\ntrue\n"),
        ("example/async_sleep.wi", "42\n"),
        ("example/async_yield.wi", "1\n2\n11\n12\n3\n"),
        ("example/async_concurrent.wi", "465\n"),
        ("example/async_preemption.wi", "42\n"),
        ("example/atomics.wi", "9\n9\n100\ntrue\n"),
        ("example/async_cooperative.wi", "1\n2\n3\n"),
        ("example/async_string_param.wi", "hello, willow\n"),
        ("example/booleans.wi", "true\nfalse\ntrue\ntrue\n"),
        ("example/classes_objects.wi", "Alice\n33\n"),
        ("example/class_hierarchy.wi", "3\n"),
        ("example/class.wi", "42\n"),
        ("example/command_line_args.wi", "0\n0\ntrue\ntrue\n"),
        ("example/constructor_visibility.wi", "pub\n42\n7\n"),
        ("example/constructors.wi", "John\n20\n7\n"),
        ("example/control_flow.wi", "120\n"),
        ("example/debug_source_map.wi", "12\n"),
        ("example/early_return.wi", "7\n0\n12\n"),
        ("example/example.wi", "50\ntrue\n"),
        ("example/fib.wi", "6765\n"),
        ("example/fib_bench.wi", "6765\n"),
        ("example/f64_parse.wi", "3.5\ntrue\nNaN\nparse failed\n"),
        ("example/floats.wi", "4\ntrue\n-4\n"),
        ("example/fn_values.wi", "20\n25\n30\n107\n104\n"),
        ("example/for_loops.wi", "6\n1\n2\n3\n5050\n9\n"),
        ("example/frozen_array.wi", "5\n4\n10\n"),
        ("example/frozen_map.wi", "3\n2\ntrue\n150\n"),
        ("example/gc_linked_list.wi", "6\n"),
        (
            "example/enum_match.wi",
            "north\nwest\n78.53975\n12\n0\nzero\nnonzero\nyes\nno\n",
        ),
        ("example/unqualified_enum_variant.wi", "42\n1007\n-1\n"),
        ("example/leibniz_pi.wi", "3.141592663589326\n"),
        ("example/locks.wi", "5\ndev\nprod\n"),
        ("example/match_color.wi", "green\n"),
        ("example/functions.wi", "25\ntrue\n"),
        ("example/hello.wi", "50"),
        ("example/hello_world.wi", "Hello, world!\n"),
        ("example/import_demo/main.wi", "30\n42\n42\n99\n3\n42\n"),
        ("example/item_import_demo/main.wi", "7\n25\n"),
        ("example/interfaces.wi", "woof\n4\ntweet\n2\nwoof\ntweet\n"),
        ("example/trait_like_interfaces.wi", "3\n4\npoint\n"),
        ("example/generic_interfaces.wi", "10\nhello\nhello\nworld\n"),
        (
            "example/generic_interface_multi_instantiation.wi",
            "file\nfile\n",
        ),
        ("example/default_methods.wi", "Hello, Rex!\nBEEP Unit-7!\n"),
        (
            "example/interface_inheritance.wi",
            "Rex / Sam\n<Rex>\n<Rex>\n",
        ),
        (
            "example/interface_downcast.wi",
            "woof\nmeow\nNemo is quiet\n",
        ),
        ("example/subclass_interface.wi", "dog\n4\npuppy\n4\n"),
        ("example/virtual_dispatch.wi", "19\n"),
        ("example/error_conversion.wi", "14\n1042\n"),
        ("example/main_result.wi", "42\n"),
        (
            "example/to_string.wi",
            "answer = 42\nok = true\npi = 3.5\np = (3, 4)\n",
        ),
        ("example/many_tasks.wi", "55\n"),
        ("example/lambda_context.wi", "20\n12\n12\nyes\n"),
        ("example/maps.wi", "2\n31\n25\n-1\ntrue\nfalse\ntwo\n"),
        ("example/module_alias_demo/main.wi", "5\n16\n"),
        ("example/module_class_demo/main.wi", "42\n12\n"),
        (
            "example/module_class_inheritance_demo/main.wi",
            "1005\n6\n1005\n",
        ),
        ("example/module_demo/main.wi", "12\n14\n"),
        ("example/module_enum_demo/main.wi", "1\n2\n42\n"),
        ("example/direct_import_demo/main.wi", "7\n1\n99\n"),
        (
            "example/direct_import_iface_enum_demo/main.wi",
            "25\n12\n3\n",
        ),
        ("example/interface_advanced_demo/main.wi", "11\n10\n42\n"),
        ("example/mutability.wi", "6\n15\ntrue\n"),
        ("example/nested_loops.wi", "30\n"),
        (
            "example/nil_guard_demo.wi",
            "42\n-7\n0\ntrue\nfalse\nfalse\n126\n99\n",
        ),
        ("example/nil_nullable.wi", "0\n10\n20\ntrue\n10\n"),
        ("example/nil_safe_chain.wi", "60\n3\n30\n-1\n120\n"),
        ("example/object_argument.wi", "42\n42\n99\n41\n"),
        (
            "example/option_result.wi",
            "true\ntrue\n10\n10\n10\n99\n20\ntrue\n2\ntrue\n42\n10\ntrue\ntrue\n8\n8\n8\n99\nsomething failed\n24\ntrue\nprefix: something failed\n8\n2\nnot even\n0\n8\n",
        ),
        (
            "example/option_result_inference.wi",
            "true\n10\ntrue\n7\n5\ntrue\n42\n-1\n",
        ),
        ("example/prot_demo.wi", "10\n9\n20\n18\n17\n15\n14\n"),
        ("example/result_propagation.wi", "84\n-1\n52\n-1\n-1\n"),
        ("example/print_test.wi", "1230\n42\ntrue\nfalsetrue\n"),
        ("example/recursion.wi", "3628800\n1024\n6\n"),
        ("example/range_value.wi", "2\n6\n4\n14\n0\n1\n2\n"),
        (
            "example/references.wi",
            "11\n22\ntrue\nhi!\nhi?\nold box\nold box!\nnew box\n3\n",
        ),
        (
            "example/rust_runtime_smoke.wi",
            "rust runtime\n42\n10\n21\n0\n",
        ),
        ("example/channel_producer.wi", "10\n20\n30\n"),
        (
            "example/concurrent_counts.wi",
            "101\n201\n301\n102\n202\n302\n103\n203\n303\n104\n204\n304\n105\n205\n305\n106\n206\n306\n107\n207\n307\n108\n208\n308\n109\n209\n309\n110\n210\n310\n1\n",
        ), // if real undeterministic concurrency is implemented, this test should be rewritten to not require a specific order of outputs
        ("example/coop_select.wi", "100\n200\n300\n"),
        ("example/parallel_tasks.wi", "55\n144\n610\n42\nfalse\n"),
        ("example/select.wi", "0\n42\n7\n"),
        ("example/self_demo.wi", "10\n10\n10\n"),
        ("example/send_sync_markers.wi", "36\n81\n"),
        ("example/spawn_join.wi", "9\n16\n25\n42\n"),
        ("example/static_inheritance.wi", "base\nbase\n3\nok\n"),
        ("example/static_members.wi", "3\n25\n40\n42\n"),
        ("example/static_mut.wi", "0\n10\n42\nstart\ndone\n"),
        (
            "example/static_properties.wi",
            "1\nwillow\ntrue\n1.5\n20\n100\n",
        ),
        ("example/std_imports.wi", "1\n42\n7\n-1\n"),
        ("example/strings.wi", "Hello, Willow\nstring concat\n"),
        ("example/string_greeting.wi", "hello, willow\ntrue\n"),
        ("example/task_sharing.wi", "6\n1\n2\n"),
        ("example/ternary.wi", "1\n-1\n0\n20\n99\n15\n8\n1\n"),
        ("example/types.wi", "10\n2.5\n10\n78.53975\ntrue\n"),
        ("example/super_class.wi", "ann\njohn\nben\n"),
        (
            "example/gc_safety_temporaries.wi",
            "Hx!\na!b!\nv!\np!q!r!\n",
        ),
        ("example/comments.wi", "30\n9223372036854775807\n"),
    ];

    let mut expected_paths = cases
        .iter()
        .map(|(path, _)| path.to_string())
        .collect::<Vec<_>>();
    expected_paths.sort();
    let actual_paths = collect_runnable_example_entries();
    assert_eq!(
        actual_paths, expected_paths,
        "every runnable non-future example entrypoint should have an output assertion"
    );

    for (path, expected) in cases {
        let (out, ok) = compile_file_and_run(path);
        assert!(ok, "{path} failed to compile or run");
        assert_eq!(out, expected, "{path} output mismatch");
    }
}

#[test]
fn test_future_examples_are_documented_not_compiled() {
    let future_examples = collect_wi_files("example/future");
    assert!(
        future_examples.len() >= 8,
        "future example catalog should stay broad"
    );

    for path in future_examples {
        let source = fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!("failed to read future example {path}: {err}");
        });

        assert!(
            source.contains("// status: future"),
            "{path} must be marked as a future example"
        );
        assert!(
            source.contains("// feature:"),
            "{path} must name the language feature it documents"
        );
    }
}

#[test]
fn test_future_example_catalog_covers_planned_features() {
    let combined = collect_wi_files("example/future")
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>()
        .join("\n");

    let required_fragments = [
        "import ", "class ", "extends ", "String", "enum ", "match ", "[i64]", "for ",
    ];

    for fragment in required_fragments {
        assert!(
            combined.contains(fragment),
            "future examples should cover `{fragment}`"
        );
    }
}

#[test]
fn test_future_example_catalog_covers_constructor_init_diagnostics() {
    let source = fs::read_to_string("example/future/diagnostic_constructor_init_rules.wi")
        .expect("missing constructor init diagnostic example");
    assert!(source.contains("// status: future"));
    assert!(source.contains("// feature: constructor init diagnostics"));
    assert!(source.contains("static init(self)"));
    assert!(source.contains("fn init(self)"));
    assert!(source.contains("static fn init()"));
}

#[test]
fn test_future_private_member_diagnostic_example_reports_only_privacy_error() {
    let stderr = compile_file_error_stderr("example/future/diagnostic_private_member.wi");
    assert!(stderr.contains("error[E0501]"), "{stderr}");
    assert!(
        stderr.contains("field `balance` of class `Account` is private"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("error[E0835]"),
        "future diagnostic example should not include stale static-call error:\n{stderr}"
    );
}

#[test]
fn test_example_readme_explains_runnable_and_future_examples() {
    let readme = fs::read_to_string("example/README.md").expect("missing example README");

    assert!(readme.contains("Root `*.wi` files"));
    assert!(readme.contains("future/**/*.wi"));
    assert!(readme.contains("// status: future"));
}

#[test]
fn test_release_example_build_runs() {
    let (out, ok) = compile_file_and_run_with_args("example/functions.wi", &["--release"]);
    assert!(ok, "release compilation failed");
    assert_eq!(out, "25\ntrue\n");
}

#[test]
fn test_debug_build_emits_source_map_sidecar() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_sourcemap_{}.wi", id));
    let bin_path = temp_path(format!("willow_sourcemap_{}", id));

    let source = r#"
fn helper(x: i64) -> i64 {
    let doubled = x * 2;
    if doubled > 10 {
        return doubled;
    }
    return doubled + 1;
}

pub class Counter {
    pub fn value(self) -> i64 {
        return 1;
    }
}

fn main() {
    println(helper(6));
}
"#;
    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("debug compilation failed: {stderr}");
    }

    let map_path = format!("{bin_path}.wsmap");
    let map = fs::read_to_string(&map_path).expect("debug build should emit a source map");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(map.contains("willow_debug_source_map_v1"));
    assert!(map.contains(&format!("file={src_path}")));
    assert!(map.contains("function name=helper"));
    assert!(map.contains("function name=Counter::value"));
    assert!(map.contains("function name=main"));
    assert!(map.contains("statement kind=let"));
    assert!(map.contains("statement kind=if"));
    assert!(map.contains("statement kind=return"));
    assert!(map.contains("statement kind=expr"));
    assert!(map.contains(" line="));
    assert!(map.contains(" col="));
}

#[test]
fn test_release_build_removes_source_map_sidecar() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_release_sourcemap_{}.wi", id));
    let bin_path = temp_path(format!("willow_release_sourcemap_{}", id));
    let map_path = format!("{bin_path}.wsmap");

    fs::write(&src_path, "fn main() { println(1); }").unwrap();
    fs::write(&map_path, "stale debug source map").unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("release compilation failed: {stderr}");
    }

    let source_map_exists = Path::new(&map_path).exists();

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(
        !source_map_exists,
        "release build should not keep {map_path}"
    );
}

#[test]
fn test_release_with_debug_info_emits_source_map_sidecar() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_release_debug_sourcemap_{}.wi", id));
    let bin_path = temp_path(format!("willow_release_debug_sourcemap_{}", id));
    let map_path = format!("{bin_path}.wsmap");

    fs::write(
        &src_path,
        r#"
fn helper() -> i64 {
    return 7;
}

fn main() {
    println(helper());
}
"#,
    )
    .unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args([
            "build",
            &src_path,
            "-o",
            &bin_path,
            "--release",
            "--debug-info",
        ])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("release-with-debug-info compilation failed: {stderr}");
    }

    let map = fs::read_to_string(&map_path).expect("release --debug-info should emit a source map");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(map.contains("willow_debug_source_map_v1"));
    assert!(map.contains(&format!("file={src_path}")));
    assert!(map.contains("function name=helper"));
    assert!(map.contains("function name=main"));
}

#[test]
fn test_debug_build_embeds_runtime_metadata_in_binary() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_runtime_metadata_{}.wi", id));
    let bin_path = temp_path(format!("willow_runtime_metadata_{}", id));

    let source = r#"
fn helper(x: i64) -> i64 {
    return x + 1;
}

pub class Counter {
    pub value: i64;

    pub fn read(self) -> i64 {
        return 1;
    }
}

fn main() {
    println(helper(41));
}
"#;
    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("debug compilation failed: {stderr}");
    }

    let binary = fs::read(&bin_path).expect("debug binary should exist");
    let metadata = String::from_utf8_lossy(&binary);

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(metadata.contains("willow_runtime_metadata_v1"));
    assert!(metadata.contains("willow_debug_source_map_v1"));
    assert!(metadata.contains(&format!("file={src_path}")));
    assert!(metadata.contains("function name=helper line="));
    assert!(metadata.contains("function name=main line="));
    assert!(metadata.contains("class name=Counter line="));
    assert!(metadata.contains("gc_type name=Counter"));
    assert!(metadata.contains("field name=value line="));
    assert!(metadata.contains("method name=read line="));
    assert!(metadata.contains("function name=Counter::read line="));
}

#[test]
fn test_debug_build_embeds_async_stack_metadata_in_binary() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_async_metadata_{}.wi", id));
    let bin_path = temp_path(format!("willow_async_metadata_{}", id));

    let source = r#"
async fn wait_value() -> i64 {
    await sleep(1);
    return 42;
}

async fn main() {
    let value = await wait_value();
    println(value);
}
"#;
    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("async debug compilation failed: {stderr}");
    }

    let binary = fs::read(&bin_path).expect("debug binary should exist");
    let metadata = String::from_utf8_lossy(&binary);

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(metadata.contains("function name=wait_value line="));
    assert!(metadata.contains("function name=main line="));
    assert!(metadata.contains("  async=true"));
    assert!(metadata.contains("  async_stack_frame name=wait_value"));
    assert!(metadata.contains("  async_stack_frame name=main"));
    assert!(metadata.contains("  await line="));
}

#[test]
fn test_debug_source_map_records_reference_params_and_call_sites() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_ref_metadata_{}.wi", id));
    let bin_path = temp_path(format!("willow_ref_metadata_{}", id));

    let source = r#"
fn read(x: & i64) -> i64 {
    return x;
}

fn bump(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut n = 1;
    println(read(&n));
    bump(&n);
}
"#;
    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("reference metadata compilation failed: {stderr}");
    }

    let map = fs::read_to_string(format!("{bin_path}.wsmap"))
        .expect("debug build should emit reference metadata");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(map.contains("function name=read line="));
    assert!(map.contains("param name=x mode=& type=i64"));
    assert!(map.contains("function name=bump line="));
    assert!(map.contains("param name=x mode=&mut type=i64"));
    assert!(
        map.contains("reference_call callee=read param=x mode=& type=i64 place_kind=local place=n")
    );
    assert!(map.contains(
        "reference_call callee=bump param=x mode=&mut type=i64 place_kind=local place=n"
    ));
}

#[test]
fn test_reference_runtime_debug_hook_reports_array_element_call_site() {
    let src = r#"
import std::collections::Array;

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut xs: Array<i64> = [1];
    increment(&xs[3]);
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "out-of-bounds reference call should abort");
    assert!(
        out.contains("array index out of bounds: the length is 1 but the index is 3"),
        "missing array bounds diagnostic:\n{out}"
    );
    assert!(
        out.contains("reference call: increment parameter `x` &mut i64"),
        "missing reference call context:\n{out}"
    );
    assert!(
        out.contains("using array_element `xs[3]`"),
        "missing referenced array element context:\n{out}"
    );
}

#[test]
fn test_release_build_omits_runtime_metadata_from_binary() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_release_runtime_metadata_{}.wi", id));
    let bin_path = temp_path(format!("willow_release_runtime_metadata_{}", id));

    fs::write(&src_path, "fn main() { println(1); }").unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("release compilation failed: {stderr}");
    }

    let binary = fs::read(&bin_path).expect("release binary should exist");
    let metadata = String::from_utf8_lossy(&binary);

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(
        !metadata.contains("willow_runtime_metadata_v1"),
        "release binary should not embed runtime metadata"
    );
}

// ---------------------------------------------------------------------------
// GC rooting under allocation stress (WILLOW_GC_STRESS=alloc).
//
// These guard codegen GC-root soundness: every live value must survive a
// collection that fires *during* a subsequent allocation.  Without the fixes
// these exercise, each crashes or prints wrong output only when a collection
// happens to land mid-expression — invisible to normal threshold-based GC.
// ---------------------------------------------------------------------------

// Enum-variant construction must root the half-built enum across argument
// evaluation: `Option::Some(Node { .. })` allocates the Node after allocating
// the Option, and that allocation can collect the unrooted Option.
#[test]
fn gc_stress_01_option_some_class_payload() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let opt = Option::Some(new Node(8));
    gc_collect();
    let v = opt.unwrap();
    println(v.get());
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "8\n");
}

// Result::Ok with a String payload through the same construction path.
#[test]
fn gc_stress_02_result_ok_string_payload() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let r: Result<String, i64> = Result::Ok("alpha");
    gc_collect();
    println(r.unwrap());
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "alpha\n");
}

// Option<String> built and matched after a collection.
#[test]
fn gc_stress_03_option_string_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let s = Option::Some("hello");
    gc_collect();
    println(s.unwrap());
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "hello\n");
}

// Fieldless (C-like) enums are immediate tags, not heap pointers, so a value of
// such an enum type must NOT be rooted/traced as a GC reference.  Passing one
// to a function that then allocates (the String literal) used to crash the
// collector by dereferencing the tag as an object header.
#[test]
fn gc_stress_04_fieldless_enum_not_rooted() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
enum Color { Red, Green, Blue, }
fn name(c: Color) -> String {
    return match c {
        Color::Red => "red",
        Color::Green => "green",
        Color::Blue => "blue",
    };
}
fn main() {
    println(name(Color::Red));
    println(name(Color::Green));
    println(name(Color::Blue));
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "red\ngreen\nblue\n");
}

// A class method returning Option, called twice.  Regression for the
// gc_root_count bookkeeping bug: the enum-construction root inside the method
// must decrement the root counter so the method epilogue does not over-pop the
// shared runtime root stack and strip the caller's live roots.
#[test]
fn gc_stress_05_class_method_returns_option_twice() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Lookup {
    pub init(self, key: i64, value: i64) {
        self.key = key;
        self.value = value;
    }
    key: i64;
    value: i64;
    pub fn find(self, k: i64) -> Option<i64> {
        if self.key == k {
            return Option::Some(self.value);
        }
        return Option::None;
    }
}
fn main() {
    let l = new Lookup(5, 100);
    println(l.find(5).unwrap());
    println(l.find(9).is_none());
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "100\ntrue\n");
}

// Enum with a payload-carrying variant IS heap-allocated and must survive a
// collection when held, including a fieldless variant (None) of the same enum.
#[test]
fn gc_stress_06_mixed_enum_variants_round_trip() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn pick(n: i64) -> Option<i64> {
    if n > 0 { return Option::Some(n * 2); }
    return Option::None;
}
fn main() {
    let mut i = 0;
    let mut total = 0;
    while i < 5 {
        let o = pick(i);
        gc_collect();
        total = total + o.unwrap_or(0);
        i = i + 1;
    }
    println(total);
}
"#,
    );
    assert!(ok, "should not crash under GC stress: {out}");
    assert_eq!(out, "20\n");
}

// Channel/Future locals are opaque RUNTIME pointers with no GC header, so
// is_gc_managed must NOT root them on the shadow stack — otherwise the collector
// reads a bogus header at payload_to_header and crashes once a collection scans
// the root (willow-lpn.9). Task/JoinHandle are GC async frames in the cooperative
// scheduler path, so it is safe and necessary to trace them.

// A spawned void function joined while collections fire on every allocation.
// The JoinHandle local is a GC frame and remains valid across collection.
#[test]
fn gc_stress_07_spawn_join_void() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn say() {
    println("hi");
}
fn main() {
    let h = say();
    gc_collect();
    h.join();
    println("done");
}
"#,
    );
    assert!(ok, "spawn/join must not crash under GC stress: {out}");
    assert_eq!(out, "hi\ndone\n");
}

// Awaiting task values of scalar types under stress. Task locals are async frame
// pointers and must remain traced across collection.
#[test]
fn gc_stress_08_task_await_scalars() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn number() -> i64 {
    return 7;
}
async fn ratio() -> f64 {
    return 2.5;
}
async fn main() {
    let f = number();
    gc_collect();
    println(await f);
    println(await ratio());
}
"#,
    );
    assert!(ok, "await must not crash under GC stress: {out}");
    assert_eq!(out, "7\n2.5\n");
}

// A channel produced by a spawned task, drained on the main task, with a
// collection between operations. The Channel local must not be traced.
#[test]
fn gc_stress_09_channel_spawn_producer() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<i64>) {
    ch.send(10);
    ch.send(20);
    ch.close();
}
fn main() {
    let ch = Channel<i64>::new();
    let h = producer(ch);
    gc_collect();
    println(ch.recv());
    println(ch.recv());
    h.join();
}
"#,
    );
    assert!(ok, "channel/spawn must not crash under GC stress: {out}");
    assert_eq!(out, "10\n20\n");
}

// ── Interface dispatch (willow-xds, spec 14) ───────────────────────────────

const IFACE_ANIMALS: &str = r#"
interface Animal {
    fn speak(self) -> String;
}
class Dog implements Animal {
    pub fn speak(self) -> String { return "woof"; }
}
class Cat implements Animal {
    pub fn speak(self) -> String { return "meow"; }
}
"#;

#[test]
fn iface_dispatch_01_basic_via_function_arg() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn say(a: Animal) {{ println(a.speak()); }}\nfn main() {{ say(new Dog()); say(new Cat()); }}"
    ));
    assert!(ok, "interface dispatch must compile and run");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_dispatch_02_local_binding() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let a: Animal = new Dog(); println(a.speak()); }}"
    ));
    assert!(ok);
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_dispatch_03_return_coercion() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn pick(b: bool) -> Animal {{ if b {{ return new Dog(); }} return new Cat(); }}\nfn main() {{ println(pick(true).speak()); println(pick(false).speak()); }}"
    ));
    assert!(ok);
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_dispatch_04_multi_method_slot_indexing() {
    // Calls both interface methods; the second exercises vtable slot 1.
    let (out, ok) = compile_and_run(
        r#"
interface Shape {
    fn name(self) -> String;
    fn area(self) -> i64;
}
class Square implements Shape {
    pub side: i64;
    pub fn name(self) -> String { return "square"; }
    pub fn area(self) -> i64 { return self.side * self.side; }
}
fn show(s: Shape) { println(s.name()); println(s.area()); }
fn main() { show(new Square(6)); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "square\n36\n");
}

#[test]
fn iface_dispatch_05_reassignment() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let mut a: Animal = new Dog(); println(a.speak()); a = new Cat(); println(a.speak()); }}"
    ));
    assert!(ok);
    assert_eq!(out, "woof\nmeow\n");
}

// spec 14.6: interface values must survive collection under GC stress.

#[test]
fn iface_gc_stress_01_local_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let a: Animal = new Dog(); gc_collect(); println(a.speak()); }}"
    ));
    assert!(ok, "interface local must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_gc_stress_02_param_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nfn say(a: Animal) {{ gc_collect(); println(a.speak()); }}\nfn main() {{ say(new Dog()); }}"
    ));
    assert!(ok, "interface parameter must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_gc_stress_03_method_result_string_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let a: Animal = new Dog(); let s = a.speak(); gc_collect(); println(s); }}"
    ));
    assert!(ok, "interface method-result String must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

// spec 14.4: a class field typed as an interface.
#[test]
fn iface_field_01_dispatch_through_field() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nclass Holder {{ pub value: Animal; }}\nfn main() {{ let h = new Holder(new Dog()); println(h.value.speak()); }}"
    ));
    assert!(ok, "interface field dispatch must work: {out}");
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_field_02_gc_stress_field_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nclass Holder {{ pub value: Animal; }}\nfn main() {{ let h = new Holder(new Dog()); gc_collect(); println(h.value.speak()); }}"
    ));
    assert!(ok, "interface field must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

// spec 14.5: Array<Interface> (empty literal + push, the documented pattern).
#[test]
fn iface_array_01_push_and_dispatch() {
    let (out, ok) = compile_and_run(&format!(
        "import std::collections::Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = []; xs.push(new Dog()); xs.push(new Cat()); println(xs[0].speak()); println(xs[1].speak()); }}"
    ));
    assert!(ok, "Array<Interface> must work: {out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_array_02_gc_stress_elements_survive() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "import std::collections::Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = []; xs.push(new Dog()); xs.push(new Cat()); gc_collect(); println(xs[0].speak()); println(xs[1].speak()); }}"
    ));
    assert!(ok, "Array<Interface> elements must survive GC: {out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_array_03_index_assign_boxes() {
    let (out, ok) = compile_and_run(&format!(
        "import std::collections::Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = []; xs.push(new Dog()); xs[0] = new Cat(); println(xs[0].speak()); }}"
    ));
    assert!(ok, "interface index-assign must box: {out}");
    assert_eq!(out, "meow\n");
}

#[test]
fn iface_array_04_nonempty_literal_with_annotation() {
    // A non-empty `Array<Interface>` literal of differing classes is checked
    // element-wise against the interface and each element is boxed.
    let (out, ok) = compile_and_run(&format!(
        "import std::collections::Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = [new Dog(), new Cat()]; println(xs[0].speak()); println(xs[1].speak()); }}"
    ));
    assert!(ok, "non-empty interface array literal must work: {out}");
    assert_eq!(out, "woof\nmeow\n");
}

// spec 11: module-qualified interface use (`animals::Animal`) where both the
// interface and the implementing class live in an imported module.
#[test]
fn iface_module_01_qualified_interface_and_class() {
    let animals = r#"
module animals;
pub interface Animal {
    fn speak(self) -> String;
}
pub class Dog implements Animal {
    pub fn speak(self) -> String { return "woof"; }
}
"#;
    let main = r#"
import animals;
fn say(a: animals::Animal) {
    println(a.speak());
}
fn main() {
    say(new animals::Dog());
    let a: animals::Animal = new animals::Dog();
    println(a.speak());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("animals.wi", animals), ("main.wi", main)], "main.wi");
    assert!(ok, "module-qualified interface project failed: {out}");
    assert_eq!(out, "woof\nwoof\n");
}

// ── Direct type imports: interface dispatch + module-body internal enum use ──
//    (willow-64gs.1)
//
// Two latent bugs are fixed and pinned here. (A) `import mod::Iface` binds the
// bare interface name, but a class records its `implements` entry under the
// qualified name `mod::Iface`; boxing/dispatch must canonicalize so the bare
// alias matches (was E0201 then a vtable-miss segfault). (B) A module function
// that uses its OWN enum internally must resolve the bare `Color::Variant` to
// `mod::Color` during module codegen, instead of silently falling back to
// variant tag 0 — this only manifests when the entry does NOT separately import
// the enum.
//
// 20 test perspectives (each pinned below; a test may cover several):
//   P1  direct iface import: concrete arg to bare `Iface` param dispatches
//   P2  direct iface import: `let x: Iface = new Cls()` then `x.method()`
//   P3  direct iface import: two classes dispatch to their own methods
//   P4  direct iface import: interface with two methods dispatches each slot
//   P5  direct iface import: `Array<Iface>` of mixed classes dispatches per elem
//   P6  negative: non-implementing class to bare `Iface` param → E0201
//   P7  regression: qualified `import mod;` + `mod::Iface` param still works
//   P8  direct iface import: iface method result used in arithmetic
//   P9  negative: direct import of a PRIVATE interface → E0419
//   P10 module fn constructs its own fieldless enum internally (not tag 0)
//   P11 module fn matches its own enum passed as a param
//   P12 entry does NOT import the enum at all; module still correct (bug cond.)
//   P13 regression: entry DOES import the enum; behavior unchanged
//   P14 module fn constructs a PAYLOAD variant internally and binds the payload
//   P15 module CLASS METHOD constructs/matches the module's own enum internally
//   P16 two enums in one module used internally — no tag cross-talk
//   P17 every variant constructed internally (Red/Green/Blue) — all tags correct
//   P18 module helper-to-helper: build enum in one fn, match it in another
//   P19 module-body interface boxing: box a local class to the module's iface
//   P20 end-to-end: direct iface dispatch + module-internal enum together

// P1, P3, P8: direct interface import, two classes, result used in arithmetic.
#[test]
fn dti_iface_01_direct_import_dispatch_two_classes() {
    let shapes = r#"
module shapes;
pub interface Area {
    fn area(self) -> i64;
}
pub class Square implements Area {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
pub class Rect implements Area {
    pub w: i64;
    pub h: i64;
    pub fn area(self) -> i64 { return self.w * self.h; }
}
"#;
    let main = r#"
import shapes::Area;
import shapes::Square;
import shapes::Rect;
fn describe(a: Area) -> i64 { return a.area() + 1; }
fn main() {
    println(describe(new Square(5)));
    println(describe(new Rect(3, 4)));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "direct interface import dispatch failed: {out}");
    assert_eq!(out, "26\n13\n");
}

// P2: direct interface import bound to a local `let` of the interface type.
#[test]
fn dti_iface_02_direct_import_let_binding_dispatch() {
    let shapes = r#"
module shapes;
pub interface Greeter {
    fn hello(self) -> String;
}
pub class En implements Greeter {
    pub fn hello(self) -> String { return "hi"; }
}
"#;
    let main = r#"
import shapes::Greeter;
import shapes::En;
fn main() {
    let g: Greeter = new En();
    println(g.hello());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "direct interface import let-binding failed: {out}");
    assert_eq!(out, "hi\n");
}

// P4: a directly-imported interface with TWO methods dispatches each method
// through the correct vtable slot (method_order indexing under the bare alias).
#[test]
fn dti_iface_03_direct_import_multi_method_dispatch() {
    let shapes = r#"
module shapes;
pub interface Pair {
    fn first(self) -> i64;
    fn second(self) -> i64;
}
pub class Point implements Pair {
    pub x: i64;
    pub y: i64;
    pub fn first(self) -> i64 { return self.x; }
    pub fn second(self) -> i64 { return self.y; }
}
"#;
    let main = r#"
import shapes::Pair;
import shapes::Point;
fn diff(p: Pair) -> i64 { return p.second() - p.first(); }
fn main() {
    println(diff(new Point(3, 10)));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "direct import multi-method dispatch failed: {out}");
    assert_eq!(out, "7\n");
}

// P5: an Array of the directly-imported interface dispatches per element.
#[test]
fn dti_iface_04_direct_import_array_dispatch() {
    let shapes = r#"
module shapes;
pub interface Area {
    fn area(self) -> i64;
}
pub class Square implements Area {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
pub class Rect implements Area {
    pub w: i64;
    pub h: i64;
    pub fn area(self) -> i64 { return self.w * self.h; }
}
"#;
    let main = r#"
import std::collections::Array;
import shapes::Area;
import shapes::Square;
import shapes::Rect;
fn main() {
    let xs: Array<Area> = [new Square(2), new Rect(3, 5)];
    let mut sum = 0;
    let mut i = 0;
    while i < xs.len() {
        sum = sum + xs[i].area();
        i = i + 1;
    }
    println(sum);
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "direct import Array<Iface> dispatch failed: {out}");
    assert_eq!(out, "19\n");
}

// P6: a class that does NOT implement the directly-imported interface is
// rejected when passed to that interface parameter (E0201 still fires).
#[test]
fn dti_iface_05_direct_import_non_impl_rejected() {
    let shapes = r#"
module shapes;
pub interface Area {
    fn area(self) -> i64;
}
pub class Square implements Area {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
pub class Tag {
    pub n: i64;
}
"#;
    let main = r#"
import shapes::Area;
import shapes::Tag;
fn describe(a: Area) -> i64 { return a.area(); }
fn main() {
    println(describe(new Tag(1)));
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(stderr.contains("error[E0201]"), "stderr: {stderr}");
}

// P7: the qualified form (module import + `mod::Iface`) is unaffected.
#[test]
fn dti_iface_06_qualified_form_regression() {
    let shapes = r#"
module shapes;
pub interface Area {
    fn area(self) -> i64;
}
pub class Square implements Area {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
"#;
    let main = r#"
import shapes;
fn describe(a: shapes::Area) -> i64 { return a.area(); }
fn main() {
    println(describe(new shapes::Square(6)));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "qualified interface form regressed: {out}");
    assert_eq!(out, "36\n");
}

// P9: directly importing a PRIVATE interface is rejected (E0419).
#[test]
fn dti_iface_07_private_interface_rejected() {
    let shapes = r#"
module shapes;
interface Secret {
    fn area(self) -> i64;
}
pub class Square {
    pub side: i64;
}
"#;
    let main = r#"
import shapes::Secret;
fn main() {
    println(1);
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(stderr.contains("error[E0419]"), "stderr: {stderr}");
}

// P10, P12, P17: a module function constructs each of its own enum's variants
// internally; the entry imports only the function (NOT the enum). Every tag must
// be correct (the bug returned tag 0 for all of them).
#[test]
fn dti_enum_01_module_internal_construction_all_variants() {
    let pal = r#"
module pal;
pub enum Color { Red, Green, Blue }
pub fn rank(c: Color) -> i64 {
    return match c {
        Color::Red => 1,
        Color::Green => 2,
        Color::Blue => 3,
    };
}
pub fn red() -> i64 { return rank(Color::Red); }
pub fn green() -> i64 { return rank(Color::Green); }
pub fn blue() -> i64 { return rank(Color::Blue); }
"#;
    let main = r#"
import pal::red;
import pal::green;
import pal::blue;
fn main() {
    println(red());
    println(green());
    println(blue());
}
"#;
    let (out, ok) = compile_temp_project_and_run(&[("pal.wi", pal), ("main.wi", main)], "main.wi");
    assert!(ok, "module-internal enum construction failed: {out}");
    assert_eq!(out, "1\n2\n3\n");
}

// P13: regression — when the entry DOES import the enum and constructs values
// itself, dispatch into the module's match is unchanged.
#[test]
fn dti_enum_02_entry_imports_enum_regression() {
    let pal = r#"
module pal;
pub enum Color { Red, Green, Blue }
pub fn rank(c: Color) -> i64 {
    return match c {
        Color::Red => 1,
        Color::Green => 2,
        Color::Blue => 3,
    };
}
"#;
    let main = r#"
import pal::Color;
import pal::rank;
fn main() {
    println(rank(Color::Red));
    println(rank(Color::Green));
    println(rank(Color::Blue));
}
"#;
    let (out, ok) = compile_temp_project_and_run(&[("pal.wi", pal), ("main.wi", main)], "main.wi");
    assert!(ok, "entry-imported enum regressed: {out}");
    assert_eq!(out, "1\n2\n3\n");
}

// P14: a module fn constructs a PAYLOAD variant internally and binds the payload
// in a match, all without the entry importing the enum.
#[test]
fn dti_enum_03_module_internal_payload_variant() {
    let pal = r#"
module pal;
pub enum Kind { Small, Big(i64) }
pub fn weigh(k: Kind) -> i64 {
    return match k {
        Kind::Small => 1,
        Kind::Big(n) => n,
    };
}
pub fn heavy() -> i64 { return weigh(Kind::Big(77)); }
pub fn light() -> i64 { return weigh(Kind::Small); }
"#;
    let main = r#"
import pal::heavy;
import pal::light;
fn main() {
    println(heavy());
    println(light());
}
"#;
    let (out, ok) = compile_temp_project_and_run(&[("pal.wi", pal), ("main.wi", main)], "main.wi");
    assert!(ok, "module-internal payload variant failed: {out}");
    assert_eq!(out, "77\n1\n");
}

// P15: a module CLASS METHOD constructs/matches the module's own enum internally
// (method bodies are compiled within the module alias scope too).
#[test]
fn dti_enum_04_module_class_method_internal_enum() {
    let pal = r#"
module pal;
pub enum Color { Red, Green, Blue }
pub class Painter {
    pub fn pick(self) -> i64 {
        let c = Color::Green;
        return match c {
            Color::Red => 1,
            Color::Green => 2,
            Color::Blue => 3,
        };
    }
}
"#;
    let main = r#"
import pal::Painter;
fn main() {
    println(new Painter().pick());
}
"#;
    let (out, ok) = compile_temp_project_and_run(&[("pal.wi", pal), ("main.wi", main)], "main.wi");
    assert!(ok, "module class-method internal enum failed: {out}");
    assert_eq!(out, "2\n");
}

// P16, P18: two enums in one module, with one fn building a value passed to
// another fn that matches it — no tag cross-talk between the two enums.
#[test]
fn dti_enum_05_two_enums_no_crosstalk() {
    let pal = r#"
module pal;
pub enum Color { Red, Green, Blue }
pub enum Size { S, M, L }
pub fn color_rank(c: Color) -> i64 {
    return match c {
        Color::Red => 1,
        Color::Green => 2,
        Color::Blue => 3,
    };
}
pub fn size_rank(s: Size) -> i64 {
    return match s {
        Size::S => 10,
        Size::M => 20,
        Size::L => 30,
    };
}
pub fn combined() -> i64 {
    let c = Color::Blue;
    let s = Size::M;
    return color_rank(c) + size_rank(s);
}
"#;
    let main = r#"
import pal::combined;
fn main() {
    println(combined());
}
"#;
    let (out, ok) = compile_temp_project_and_run(&[("pal.wi", pal), ("main.wi", main)], "main.wi");
    assert!(ok, "two-enum module no-crosstalk failed: {out}");
    // Blue (3) + M (20) = 23
    assert_eq!(out, "23\n");
}

// P11, P19: a module function boxes a local class to the module's OWN interface
// and dispatches internally; the entry only calls the function. Exercises
// module-body interface boxing under the alias scope.
#[test]
fn dti_iface_08_module_internal_interface_boxing() {
    let shapes = r#"
module shapes;
pub interface Area {
    fn area(self) -> i64;
}
pub class Square implements Area {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
fn measure(a: Area) -> i64 { return a.area(); }
pub fn run() -> i64 {
    let a: Area = new Square(9);
    return measure(a);
}
"#;
    let main = r#"
import shapes::run;
fn main() {
    println(run());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "module-internal interface boxing failed: {out}");
    assert_eq!(out, "81\n");
}

// P20: end-to-end — direct interface dispatch from the entry AND a module
// function that uses its own enum internally, in one project.
#[test]
fn dti_combined_01_iface_dispatch_plus_internal_enum() {
    let shapes = r#"
module shapes;
pub interface Drawable {
    fn area(self) -> i64;
}
pub class Square implements Drawable {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
pub enum Color { Red, Green, Blue }
pub fn rank(c: Color) -> i64 {
    return match c {
        Color::Red => 1,
        Color::Green => 2,
        Color::Blue => 3,
    };
}
pub fn brightest() -> i64 { return rank(Color::Blue); }
"#;
    let main = r#"
import shapes::Drawable;
import shapes::Square;
import shapes::brightest;
fn total(a: Drawable) -> i64 { return a.area(); }
fn main() {
    println(total(new Square(5)));
    println(brightest());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "combined direct-import iface + internal enum failed: {out}"
    );
    assert_eq!(out, "25\n3\n");
}

// ── Advanced interface features: cross-module inheritance, default methods,
//    generic interfaces, Self resolution (willow-1js.5 / .7 / .8) ──
//
// 23 test perspectives (each pinned below; a test may cover several):
//   P1  cross-module interface inheritance: entry class implements module Sub
//   P2  cross-module: a Sub value is usable where its Super is expected
//   P3  cross-module transitive inheritance (C extends B extends A)
//   P4  within-module inheritance: module class implements a module sub-interface
//   P5  multiple super-interfaces compose and dispatch through every parent
//   P6  interface `extends` cycle still rejected (E0423)
//   P7  cross-module default method inherited by an entry class
//   P8  cross-module default inherited by a class in ANOTHER module
//   P9  unimplemented interface default body is type-checked (bad body -> error)
//   P10 unimplemented interface default body that is valid -> no error
//   P11 ambiguous default from two independent interfaces -> E0425
//   P12 ambiguity resolved by an explicit class override -> ok
//   P13 same-named default where one interface extends the other -> sub wins
//   P14 no duplicate diagnostic for a bad non-generic default that IS implemented
//   P15 generic-interface default with type-arg substitution (`dup` returns i64)
//   P16 generic-interface default returning `Self`
//   P17 module-internal NON-generic interface param (entry imports fn, not iface)
//   P18 module-internal GENERIC interface param (entry imports fn, not iface)
//   P19 `Self` on a generic interface-typed receiver keeps type args (`Box<i64>`)
//   P20 qualified cross-module generic interface (`import m; m::Box<i64>`)
//   P21 direct-import cross-module generic interface (`import m::Box`)
//   P22 default body calls another (required) interface method via `self`
//   P23 cross-module default + inheritance together in one entry class
//   P24 multiple super-interfaces with conflicting inherited defaults -> E0425
//   P25 child interface default resolves inherited default ambiguity
//   P26 diamond inheritance of one shared default is not ambiguous
//   P27 cross-module inherited default ambiguity is rejected

// P1, P2: cross-module interface inheritance + usable-as-super.
#[test]
fn iface_adv_01_cross_module_inheritance() {
    let proto = r#"
module proto;
pub interface Named { fn name(self) -> i64; }
pub interface Greeter extends Named { fn greet(self) -> i64; }
"#;
    let main = r#"
import proto::Named;
import proto::Greeter;
class En implements Greeter {
    pub fn name(self) -> i64 { return 10; }
    pub fn greet(self) -> i64 { return 20; }
}
fn who(n: Named) -> i64 { return n.name(); }
fn main() {
    let g: Greeter = new En();
    println(g.greet());
    println(who(g));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(ok, "cross-module interface inheritance failed: {out}");
    assert_eq!(out, "20\n10\n");
}

// P3: transitive cross-module inheritance (C extends B extends A).
#[test]
fn iface_adv_02_cross_module_transitive_inheritance() {
    let proto = r#"
module proto;
pub interface A { fn a(self) -> i64; }
pub interface B extends A { fn b(self) -> i64; }
pub interface C extends B { fn c(self) -> i64; }
"#;
    let main = r#"
import proto::A;
import proto::C;
class Impl implements C {
    pub fn a(self) -> i64 { return 1; }
    pub fn b(self) -> i64 { return 2; }
    pub fn c(self) -> i64 { return 3; }
}
fn top(a: A) -> i64 { return a.a(); }
fn main() {
    let x: C = new Impl();
    println(top(x));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(ok, "transitive cross-module inheritance failed: {out}");
    assert_eq!(out, "1\n");
}

// P4: a class defined IN a module implements a module sub-interface and is used
// internally; the entry only calls a module function.
#[test]
fn iface_adv_03_within_module_inheritance() {
    let proto = r#"
module proto;
pub interface A { fn a(self) -> i64; }
pub interface B extends A { fn b(self) -> i64; }
pub class Impl implements B {
    pub fn a(self) -> i64 { return 7; }
    pub fn b(self) -> i64 { return 8; }
}
pub fn run() -> i64 {
    let x: B = new Impl();
    return x.a() + x.b();
}
"#;
    let main = r#"
import proto::run;
fn main() { println(run()); }
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(ok, "within-module inheritance failed: {out}");
    assert_eq!(out, "15\n");
}

// P5: more than one super-interface composes all parent requirements; a class
// implementing the child is usable as each parent and as the child.
#[test]
fn iface_adv_04_multiple_supers_compose_and_dispatch() {
    let proto = r#"
module proto;
pub interface A { fn a(self) -> i64; }
pub interface B { fn b(self) -> i64; }
pub interface C extends A, B { fn c(self) -> i64; }
"#;
    let main = r#"
import proto::A;
import proto::B;
import proto::C;

class Impl implements C {
    pub fn a(self) -> i64 { return 1; }
    pub fn b(self) -> i64 { return 2; }
    pub fn c(self) -> i64 { return 3; }
}

fn from_a(a: A) -> i64 { return a.a(); }
fn from_b(b: B) -> i64 { return b.b(); }
fn from_c(c: C) -> i64 { return c.a() + c.b() + c.c(); }

fn main() {
    let value = new Impl();
    println(from_a(value));
    println(from_b(value));
    println(from_c(value));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(ok, "multiple super-interface dispatch failed");
    assert_eq!(out, "1\n2\n6\n");
}

// P6: an `extends` cycle is rejected.
#[test]
fn iface_adv_05_extends_cycle_rejected() {
    assert_compile_error_contains(
        "interface A extends B {}\ninterface B extends A {}\nfn main() {}\n",
        &["error[E0423]"],
    );
}

// P7, P23: cross-module default method inherited by an entry class, alongside
// inheritance.
#[test]
fn iface_adv_06_cross_module_default_method() {
    let proto = r#"
module proto;
pub interface Describable {
    fn label(self) -> i64;
    fn describe(self) -> i64 { return self.label() + 100; }
}
"#;
    let main = r#"
import proto::Describable;
class Item implements Describable {
    pub fn label(self) -> i64 { return 5; }
}
fn main() {
    let d: Describable = new Item();
    println(d.describe());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(ok, "cross-module default method failed: {out}");
    assert_eq!(out, "105\n");
}

// P8: a class defined in a SECOND module inherits a default from a FIRST module's
// interface.
#[test]
fn iface_adv_07_cross_module_default_in_other_module() {
    let proto = r#"
module proto;
pub interface Describable {
    fn label(self) -> i64;
    fn describe(self) -> i64 { return self.label() + 1; }
}
"#;
    let impls = r#"
module impls;
import proto::Describable;
pub class Item implements Describable {
    pub fn label(self) -> i64 { return 41; }
}
pub fn run() -> i64 {
    let d: Describable = new Item();
    return d.describe();
}
"#;
    let main = r#"
import impls::run;
fn main() { println(run()); }
"#;
    let (out, ok) = compile_temp_project_and_run(
        &[("proto.wi", proto), ("impls.wi", impls), ("main.wi", main)],
        "main.wi",
    );
    assert!(ok, "cross-module default in other module failed: {out}");
    assert_eq!(out, "42\n");
}

// P9: an unimplemented interface's default body with a type error is reported.
#[test]
fn iface_adv_08_unimplemented_default_body_checked() {
    assert_compile_error_contains(
        "interface Foo { fn bar(self) -> i64 { return true; } }\nfn main() { println(1); }\n",
        &["error[E0201]"],
    );
}

// P10: a valid unimplemented default body compiles cleanly.
#[test]
fn iface_adv_09_unimplemented_default_body_ok() {
    let (out, ok) = compile_and_run(
        "interface Foo { fn bar(self) -> i64 { return 1; } }\nfn main() { println(7); }\n",
    );
    assert!(ok, "valid unimplemented default body must compile: {out}");
    assert_eq!(out, "7\n");
}

// P11: two independent interfaces with a same-named default is ambiguous (E0425).
#[test]
fn iface_adv_10_ambiguous_default_rejected() {
    assert_compile_error_contains(
        "interface A { fn tag(self) -> i64 { return 1; } }\ninterface B { fn tag(self) -> i64 { return 2; } }\nclass C implements A, B {}\nfn main() { println(new C().tag()); }\n",
        &["error[E0425]"],
    );
}

// P12: an explicit override resolves the ambiguity.
#[test]
fn iface_adv_11_ambiguous_default_resolved_by_override() {
    let (out, ok) = compile_and_run(
        "interface A { fn tag(self) -> i64 { return 1; } }\ninterface B { fn tag(self) -> i64 { return 2; } }\nclass C implements A, B {\n    pub fn tag(self) -> i64 { return 9; }\n}\nfn main() { println(new C().tag()); }\n",
    );
    assert!(ok, "override should resolve ambiguity: {out}");
    assert_eq!(out, "9\n");
}

// P13: a same-named default where one interface extends the other is NOT
// ambiguous; the sub-interface's default wins.
#[test]
fn iface_adv_12_hierarchy_default_not_ambiguous() {
    let (out, ok) = compile_and_run(
        "interface A { fn tag(self) -> i64 { return 1; } }\ninterface B extends A { fn tag(self) -> i64 { return 2; } }\nclass C implements B {}\nfn main() { println(new C().tag()); }\n",
    );
    assert!(ok, "hierarchy default should not be ambiguous: {out}");
    assert_eq!(out, "2\n");
}

// P24: a child interface that inherits conflicting defaults from independent
// super-interfaces is ambiguous, even when the class implements only the child.
#[test]
fn iface_adv_13b_multiple_super_inherited_default_conflict_rejected() {
    assert_compile_error_contains(
        "interface A { fn tag(self) -> i64 { return 1; } }\ninterface B { fn tag(self) -> i64 { return 2; } }\ninterface C extends A, B {}\nclass Impl implements C {}\nfn main() { println(new Impl().tag()); }\n",
        &["error[E0425]"],
    );
}

// P27: the same ambiguity is rejected when the conflicting child interface is
// declared in an imported module.
#[test]
fn iface_adv_13b_cross_module_inherited_default_conflict_rejected() {
    let proto = r#"
module proto;
pub interface A { fn tag(self) -> i64 { return 1; } }
pub interface B { fn tag(self) -> i64 { return 2; } }
pub interface C extends A, B {}
"#;
    let main = r#"
import proto::C;
class Impl implements C {}
fn main() { println(new Impl().tag()); }
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(
        stderr.contains("error[E0425]"),
        "expected inherited default conflict: {stderr}"
    );
}

// P25: a child interface can resolve inherited default ambiguity by declaring
// the method itself.
#[test]
fn iface_adv_13c_multiple_super_inherited_default_resolved_by_child_default() {
    let (out, ok) = compile_and_run(
        "interface A { fn tag(self) -> i64 { return 1; } }\ninterface B { fn tag(self) -> i64 { return 2; } }\ninterface C extends A, B { fn tag(self) -> i64 { return 3; } }\nclass Impl implements C {}\nfn main() { println(new Impl().tag()); }\n",
    );
    assert!(
        ok,
        "child interface default should resolve ambiguity: {out}"
    );
    assert_eq!(out, "3\n");
}

// P26: two paths to the same inherited default (diamond shape) should still be
// one implementation, not an ambiguity.
#[test]
fn iface_adv_13d_diamond_shared_default_not_ambiguous() {
    let (out, ok) = compile_and_run(
        "interface Root { fn tag(self) -> i64 { return 7; } }\ninterface Left extends Root {}\ninterface Right extends Root {}\ninterface Join extends Left, Right {}\nclass Impl implements Join {}\nfn main() { println(new Impl().tag()); }\n",
    );
    assert!(ok, "shared diamond default should not be ambiguous: {out}");
    assert_eq!(out, "7\n");
}

// P14: a bad non-generic default that IS implemented reports exactly once.
#[test]
fn iface_adv_13_implemented_bad_default_single_diagnostic() {
    let stderr = compile_error_stderr(
        "interface Foo { fn bar(self) -> i64 { return true; } }\nclass C implements Foo {}\nfn main() { println(1); }\n",
    );
    let count = stderr.matches("error[E0201]").count();
    assert_eq!(
        count, 1,
        "expected exactly one E0201, got {count}: {stderr}"
    );
}

// P15: a generic interface default substitutes the type parameter (`dup` -> i64).
#[test]
fn iface_adv_14_generic_default_substitution() {
    let (out, ok) = compile_and_run(
        "interface Box<T> {\n    fn get(self) -> T;\n    fn dup(self) -> T { return self.get(); }\n}\nclass IntBox implements Box<i64> {\n    value: i64;\n    pub init(self, value: i64) { self.value = value; }\n    pub fn get(self) -> i64 { return self.value; }\n}\nfn main() { println(new IntBox(42).dup()); }\n",
    );
    assert!(ok, "generic default substitution failed: {out}");
    assert_eq!(out, "42\n");
}

// P16, P19: `Self` on a generic interface-typed receiver keeps its type args, and
// a `Self`-returning method dispatched through the interface re-boxes correctly.
#[test]
fn iface_adv_15_self_on_generic_interface_receiver() {
    let (out, ok) = compile_and_run(
        "interface Box<T> {\n    fn get(self) -> T;\n    fn copy(self) -> Self;\n}\nclass IntBox implements Box<i64> {\n    value: i64;\n    pub init(self, value: i64) { self.value = value; }\n    pub fn get(self) -> i64 { return self.value; }\n    pub fn copy(self) -> IntBox { return new IntBox(self.value); }\n}\nfn main() {\n    let b: Box<i64> = new IntBox(5);\n    let c: Box<i64> = b.copy();\n    println(c.get());\n}\n",
    );
    assert!(ok, "Self on generic interface receiver failed: {out}");
    assert_eq!(out, "5\n");
}

// P17: a module function with a NON-generic interface parameter, where the entry
// imports only the function and the implementing class (not the interface).
#[test]
fn iface_adv_16_module_fn_nongeneric_interface_param() {
    let proto = r#"
module proto;
pub interface Named { fn name(self) -> i64; }
pub class Tag implements Named {
    pub v: i64;
    pub fn name(self) -> i64 { return self.v; }
}
pub fn id_of(n: Named) -> i64 { return n.name(); }
"#;
    let main = r#"
import proto::Tag;
import proto::id_of;
fn main() { println(id_of(new Tag(5))); }
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(ok, "module fn non-generic interface param failed: {out}");
    assert_eq!(out, "5\n");
}

// P18: a module function with a GENERIC interface parameter, entry imports only
// the function and the class.
#[test]
fn iface_adv_17_module_fn_generic_interface_param() {
    let boxmod = r#"
module boxmod;
pub interface Box<T> { fn get(self) -> T; }
pub class IntBox implements Box<i64> {
    pub v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
pub fn unwrap(b: Box<i64>) -> i64 { return b.get(); }
"#;
    let main = r#"
import boxmod::IntBox;
import boxmod::unwrap;
fn main() { println(unwrap(new IntBox(9))); }
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("boxmod.wi", boxmod), ("main.wi", main)], "main.wi");
    assert!(ok, "module fn generic interface param failed: {out}");
    assert_eq!(out, "9\n");
}

// P20: qualified cross-module generic interface (`import m; m::Box<i64>`).
#[test]
fn iface_adv_18_qualified_cross_module_generic_interface() {
    let boxmod = r#"
module boxmod;
pub interface Box<T> { fn get(self) -> T; }
"#;
    let main = r#"
import boxmod;
class IntBox implements boxmod::Box<i64> {
    value: i64;
    pub init(self, value: i64) { self.value = value; }
    pub fn get(self) -> i64 { return self.value; }
}
fn show(b: boxmod::Box<i64>) -> i64 { return b.get(); }
fn main() {
    let b: boxmod::Box<i64> = new IntBox(7);
    println(show(b));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("boxmod.wi", boxmod), ("main.wi", main)], "main.wi");
    assert!(ok, "qualified cross-module generic interface failed: {out}");
    assert_eq!(out, "7\n");
}

// P21: direct-import cross-module generic interface (`import m::Box`).
#[test]
fn iface_adv_19_direct_import_cross_module_generic_interface() {
    let boxmod = r#"
module boxmod;
pub interface Box<T> { fn get(self) -> T; }
"#;
    let main = r#"
import boxmod::Box;
class IntBox implements Box<i64> {
    value: i64;
    pub init(self, value: i64) { self.value = value; }
    pub fn get(self) -> i64 { return self.value; }
}
fn show(b: Box<i64>) -> i64 { return b.get(); }
fn main() {
    let b: Box<i64> = new IntBox(7);
    println(show(b));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("boxmod.wi", boxmod), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "direct-import cross-module generic interface failed: {out}"
    );
    assert_eq!(out, "7\n");
}

// P22: a default body that calls another (required) interface method via `self`.
#[test]
fn iface_adv_20_default_calls_required_method() {
    let (out, ok) = compile_and_run(
        "interface Counter {\n    fn base(self) -> i64;\n    fn doubled(self) -> i64 { return self.base() * 2; }\n}\nclass C implements Counter {\n    pub fn base(self) -> i64 { return 21; }\n}\nfn main() {\n    let c: Counter = new C();\n    println(c.doubled());\n}\n",
    );
    assert!(ok, "default calling required method failed: {out}");
    assert_eq!(out, "42\n");
}

// ── Async frame: frame-backed GC params survive across await (willow-lpn.5a) ──

#[test]
fn async_frame_01_string_param_across_await() {
    let (out, ok) = compile_and_run(
        r#"
async fn echo(s: String) -> String {
    await sleep(1);
    return s;
}
async fn main() {
    println(await echo("hello"));
}
"#,
    );
    assert!(ok, "async String param across await must work: {out}");
    assert_eq!(out, "hello\n");
}

#[test]
fn async_frame_02_second_param_slot_indexing() {
    // Returning the second GC param verifies per-slot frame offsets.
    let (out, ok) = compile_and_run(
        r#"
async fn pick(a: String, b: String) -> String {
    await sleep(1);
    return b;
}
async fn main() {
    println(await pick("first", "second"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "second\n");
}

#[test]
fn async_frame_03_mixed_gc_and_scalar_params() {
    // A non-GC param (slot 0) stays on the stack; the GC param (slot 1) is
    // frame-backed — exercises slot-indexed offsets independent of which slots
    // are frame-backed.
    let (out, ok) = compile_and_run(
        r#"
async fn pick(n: i64, s: String) -> String {
    await sleep(1);
    return s;
}
async fn main() {
    println(await pick(7, "kept"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "kept\n");
}

#[test]
fn async_frame_04_gc_stress_param_survives() {
    // The String param is reachable only through the heap frame across the
    // await; it must survive collection at every allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn echo(s: String) -> String {
    await sleep(1);
    return s;
}
async fn main() {
    println(await echo("hello world"));
}
"#,
    );
    assert!(ok, "frame-backed param must survive GC stress: {out}");
    assert_eq!(out, "hello world\n");
}

#[test]
fn async_frame_05_annotated_string_local_across_await() {
    let (out, ok) = compile_and_run(
        r#"
async fn make() -> String {
    let s: String = "local value";
    await sleep(1);
    return s;
}
async fn main() {
    println(await make());
}
"#,
    );
    assert!(ok, "annotated GC local across await must work: {out}");
    assert_eq!(out, "local value\n");
}

#[test]
fn async_frame_06_mutated_frame_local_round_trips() {
    // The local is read+written on both sides of the await; values must round
    // trip through the heap frame slot.
    let (out, ok) = compile_and_run(
        r#"
async fn build() -> String {
    let mut s: String = "a";
    s = s + "b";
    await sleep(1);
    s = s + "c";
    return s;
}
async fn main() {
    println(await build());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "abc\n");
}

#[test]
fn async_frame_07_gc_stress_local_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn make() -> String {
    let s: String = "kept across await";
    await sleep(1);
    return s;
}
async fn main() {
    println(await make());
}
"#,
    );
    assert!(ok, "frame-backed local must survive GC stress: {out}");
    assert_eq!(out, "kept across await\n");
}

// ── lpn.5c slice 1: unannotated locals frame-backed via type-checker types ──

#[test]
fn async_frame_08_unannotated_local_across_await() {
    let (out, ok) = compile_and_run(
        r#"
async fn make() -> String {
    let s = "unannotated";
    await sleep(1);
    return s;
}
async fn main() {
    println(await make());
}
"#,
    );
    assert!(ok, "unannotated GC local across await must work: {out}");
    assert_eq!(out, "unannotated\n");
}

#[test]
fn async_frame_09_unannotated_local_mutated_round_trips() {
    let (out, ok) = compile_and_run(
        r#"
async fn build() -> String {
    let mut s = "x";
    await sleep(1);
    s = s + "y";
    return s;
}
async fn main() {
    println(await build());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "xy\n");
}

#[test]
fn async_frame_10_unannotated_local_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn make() -> String {
    let s = "inferred kept";
    await sleep(1);
    return s;
}
async fn main() {
    println(await make());
}
"#,
    );
    assert!(
        ok,
        "unannotated frame-backed local must survive GC stress: {out}"
    );
    assert_eq!(out, "inferred kept\n");
}

// ── Frame-backed values across await: GC tracing by type (lpn.5c perspectives) ──
// Each value lives ONLY in the GC-rooted heap frame across the await, so these
// verify the frame's per-type GC tracing under collection at every allocation.

#[test]
fn async_frame_11_class_with_ref_field_survives() {
    // Two-level tracing: frame traces the Box, Box's mask traces its String field.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Box { pub s: String; }
async fn f() -> String {
    let b: Box = new Box("nested");
    await sleep(1);
    return b.s;
}
async fn main() { println(await f()); }
"#,
    );
    assert!(ok, "class with ref field must survive across await: {out}");
    assert_eq!(out, "nested\n");
}

#[test]
fn async_frame_12_array_of_string_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn f() -> String {
    let xs: Array<String> = [];
    xs.push("e0");
    xs.push("e1");
    await sleep(1);
    return xs[1];
}
async fn main() { println(await f()); }
"#,
    );
    assert!(ok, "Array<String> must survive across await: {out}");
    assert_eq!(out, "e1\n");
}

#[test]
fn async_frame_13_option_payload_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn f() -> String {
    let o: Option<String> = Option::Some("opt");
    await sleep(1);
    return match o { Option::Some(x) => x, Option::None => "none", };
}
async fn main() { println(await f()); }
"#,
    );
    assert!(
        ok,
        "Option<String> payload must survive across await: {out}"
    );
    assert_eq!(out, "opt\n");
}

#[test]
fn async_frame_14_result_payload_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn f() -> String {
    let r: Result<String, String> = Result::Ok("ok");
    await sleep(1);
    return match r { Result::Ok(x) => x, Result::Err(e) => e, };
}
async fn main() { println(await f()); }
"#,
    );
    assert!(ok, "Result payload must survive across await: {out}");
    assert_eq!(out, "ok\n");
}

#[test]
fn async_frame_15_map_ref_value_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Map;

async fn f() -> String {
    let mut m: Map<String, String> = Map::new();
    m.insert("k", "val");
    await sleep(1);
    return match m.get("k") { Option::Some(v) => v, Option::None => "missing", };
}
async fn main() { println(await f()); }
"#,
    );
    assert!(ok, "Map ref value must survive across await: {out}");
    assert_eq!(out, "val\n");
}

#[test]
fn async_frame_16_nullable_non_nil_survives() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Node { pub value: i64; pub next: Node?; }
async fn f(n: Node?) -> i64 {
    await sleep(1);
    if n == nil { return -1; }
    return n.value;
}
async fn main() { println(await f(new Node(77, nil))); }
"#,
    );
    assert!(ok, "non-nil nullable must survive across await: {out}");
    assert_eq!(out, "77\n");
}

#[test]
fn async_frame_17_nullable_nil_traced_as_null() {
    // A nil nullable in a GC frame slot must be skipped (not dereferenced) by the
    // collector, not crash.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Node { pub value: i64; pub next: Node?; }
async fn f(n: Node?) -> i64 {
    await sleep(1);
    if n == nil { return -1; }
    return n.value;
}
async fn main() { println(await f(nil)); }
"#,
    );
    assert!(
        ok,
        "nil nullable frame slot must be safe across await: {out}"
    );
    assert_eq!(out, "-1\n");
}

#[test]
fn async_frame_18_task_local_traced_across_await() {
    // A Task local held across an await is a GC async-frame pointer; it must be
    // traced as a heap object and remain awaitable after collection.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn other() -> i64 { return 7; }
async fn f() -> i64 {
    let fut = other();
    await sleep(1);
    return await fut;
}
async fn main() { println(await f()); }
"#,
    );
    assert!(
        ok,
        "Task local across await must stay alive across collection: {out}"
    );
    assert_eq!(out, "7\n");
}

#[test]
fn async_frame_19_join_handle_local_not_traced_across_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn work() { println("worked"); }
async fn f() {
    let h = work();
    await sleep(1);
    h.join();
}
async fn main() { await f(); }
"#,
    );
    assert!(
        ok,
        "JoinHandle local across await must not crash the collector: {out}"
    );
    assert_eq!(out, "worked\n");
}

#[test]
fn async_frame_20_channel_local_not_traced_across_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<i64>) { ch.send(11); ch.close(); }
async fn f() -> i64 {
    let ch = Channel<i64>::new();
    let h = producer(ch);
    await sleep(1);
    let v = ch.recv();
    h.join();
    return v;
}
async fn main() { println(await f()); }
"#,
    );
    assert!(
        ok,
        "Channel local across await must not crash the collector: {out}"
    );
    assert_eq!(out, "11\n");
}

// ── Module-qualified type visibility (willow-7ihl): E0419 for private types ──

#[test]
fn module_vis_01_private_class_annotation_rejected() {
    let m = "module animals;\nclass Secret { pub v: i64; }\npub class Dog {}\n";
    let main = "import animals;\nfn main() { let s: animals::Secret = new animals::Secret(5); println(s.v); }\n";
    let stderr =
        compile_temp_project_error_stderr(&[("animals.wi", m), ("main.wi", main)], "main.wi");
    assert!(stderr.contains("E0419"), "expected E0419, got: {stderr}");
    assert!(
        stderr.contains("private"),
        "diagnostic should mention private: {stderr}"
    );
}

#[test]
fn module_vis_02_pub_class_accessible() {
    let m = "module animals;\nclass Secret { pub v: i64; }\npub class Dog { pub fn speak(self) -> i64 { return 1; } }\n";
    let main = "import animals;\nfn main() { let d: animals::Dog = new animals::Dog(); println(d.speak()); }\n";
    let (out, ok) =
        compile_temp_project_and_run(&[("animals.wi", m), ("main.wi", main)], "main.wi");
    assert!(ok, "pub module class must be accessible: {out}");
    assert_eq!(out, "1\n");
}

#[test]
fn module_vis_03_private_interface_rejected() {
    let m = "module animals;\ninterface Hidden { fn f(self) -> i64; }\npub interface Shown { fn f(self) -> i64; }\n";
    let main = "import animals;\nfn use_it(a: animals::Hidden) {}\nfn main() {}\n";
    let stderr =
        compile_temp_project_error_stderr(&[("animals.wi", m), ("main.wi", main)], "main.wi");
    assert!(stderr.contains("E0419"), "expected E0419, got: {stderr}");
    assert!(
        stderr.contains("interface"),
        "diagnostic should name the kind: {stderr}"
    );
}

#[test]
fn module_vis_04_private_class_static_call_rejected() {
    let m = "module animals;\nclass Secret { pub static fn make() -> i64 { return 9; } }\npub class Dog {}\n";
    let main = "import animals;\nfn main() { println(animals::Secret::make()); }\n";
    let stderr =
        compile_temp_project_error_stderr(&[("animals.wi", m), ("main.wi", main)], "main.wi");
    assert!(
        stderr.contains("E0419"),
        "expected E0419 on static call, got: {stderr}"
    );
}

#[test]
fn module_vis_05_pub_interface_accessible() {
    let m = "module shapes;\npub interface Shape { fn area(self) -> i64; }\npub class Sq implements Shape { pub side: i64; pub fn area(self) -> i64 { return self.side * self.side; } }\n";
    let main = "import shapes;\nfn describe(s: shapes::Shape) { println(s.area()); }\nfn main() { describe(new shapes::Sq(4)); }\n";
    let (out, ok) = compile_temp_project_and_run(&[("shapes.wi", m), ("main.wi", main)], "main.wi");
    assert!(ok, "pub module interface must be accessible: {out}");
    assert_eq!(out, "16\n");
}

// ── fn main() -> Result<void, E> (willow-exg) ────────────────────────────────

#[test]
fn main_result_01_err_prints_and_exits_nonzero() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() -> Result<void, String> {
    return Result::Err("boom");
}
"#,
    );
    assert!(!ok, "Err main must exit non-zero");
    assert!(
        out.contains("boom"),
        "Err report must include the message: {out}"
    );
}

#[test]
fn main_result_02_ok_exits_zero() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() -> Result<void, String> {
    println(7);
    return Result::Ok();
}
"#,
    );
    assert!(ok, "Ok main must exit 0: {out}");
    assert_eq!(out, "7\n");
}

#[test]
fn main_result_03_implicit_end_is_success() {
    // Falling off the end of a Result<void,E> main is success (exit 0).
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() -> Result<void, String> {
    println(99);
}
"#,
    );
    assert!(ok, "implicit-end main must exit 0: {out}");
    assert_eq!(out, "99\n");
}

#[test]
fn main_result_04_question_mark_propagates_err() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn risky(ok: bool) -> Result<i64, String> {
    if ok { return Result::Ok(7); }
    return Result::Err("propagated");
}
fn main() -> Result<void, String> {
    let x = risky(false)?;
    println(x);
    return Result::Ok();
}
"#,
    );
    assert!(!ok, "? propagating Err must exit non-zero");
    assert!(
        out.contains("propagated"),
        "should report the propagated error: {out}"
    );
}

#[test]
fn main_result_05_question_mark_success_path() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn risky(ok: bool) -> Result<i64, String> {
    if ok { return Result::Ok(7); }
    return Result::Err("nope");
}
fn main() -> Result<void, String> {
    let x = risky(true)?;
    println(x);
    return Result::Ok();
}
"#,
    );
    assert!(ok, "? success path must exit 0: {out}");
    assert_eq!(out, "7\n");
}

#[test]
fn main_result_06_non_string_error_exits_nonzero() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() -> Result<void, i64> {
    return Result::Err(42);
}
"#,
    );
    assert!(!ok, "non-String Err main must still exit non-zero: {out}");
}

// ── Explicit toString() + String-only concatenation (willow-fvfc) ────────────

#[test]
fn tostring_01_primitives_and_class() {
    let (out, ok) = compile_and_run(
        r#"
class Point { pub x: i64; pub y: i64;
    pub fn toString(self) -> String { return "(" + self.x.toString() + ", " + self.y.toString() + ")"; }
}
fn main() {
    println(42.toString());
    println(true.toString());
    println(false.toString());
    println(3.5.toString());
    println("hi".toString());
    println("x = " + 42.toString());
    let p = new Point(3, 4);
    println(p.toString());
}
"#,
    );
    assert!(ok, "toString must compile and run: {out}");
    assert_eq!(out, "42\ntrue\nfalse\n3.5\nhi\nx = 42\n(3, 4)\n");
}

#[test]
fn tostring_02_gc_stress() {
    // toString allocates WillowStrings; concatenation chains must stay GC-safe.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let n = 7;
    let s = "n=" + n.toString() + " ok=" + true.toString();
    println(s);
}
"#,
    );
    assert!(ok, "toString/concat must survive GC stress: {out}");
    assert_eq!(out, "n=7 ok=true\n");
}

#[test]
fn tostring_03_string_plus_int_rejected() {
    assert_compile_error_contains(
        r#"fn main() { println("x = " + 42); }"#,
        &["error[E0202]", "cannot concatenate", ".toString()"],
    );
}

#[test]
fn tostring_04_int_plus_string_rejected() {
    assert_compile_error_contains(
        r#"fn main() { let s: String = "y"; println(42 + s); }"#,
        &["error[E0202]", "cannot concatenate"],
    );
}

#[test]
fn tostring_05_string_plus_bool_and_f64_rejected() {
    assert_compile_error_contains(
        r#"fn main() { println("b = " + true); }"#,
        &["error[E0202]", "cannot concatenate `String` with `bool`"],
    );
    assert_compile_error_contains(
        r#"fn main() { println("f = " + 3.5); }"#,
        &["error[E0202]", "cannot concatenate `String` with `f64`"],
    );
}

// ── panic() builtin (regression: codegen no longer crashes; willow-4j6) ──────

#[test]
fn panic_01_compiles_runs_and_exits_nonzero() {
    let (out, ok) = compile_and_run_check_exit(r#"fn main() { panic("boom"); }"#);
    assert!(!ok, "panic must exit non-zero");
    assert!(
        out.contains("boom"),
        "panic should print its message: {out}"
    );
}

#[test]
fn panic_02_in_nested_function() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn deeper() { panic("deep failure"); }
fn helper() { deeper(); }
fn main() { helper(); }
"#,
    );
    assert!(!ok, "panic in a nested call must exit non-zero");
    assert!(
        out.contains("deep failure"),
        "panic message must appear: {out}"
    );
}

#[test]
fn panic_03_debug_build_reports_source_location() {
    // Debug builds include the panic call-site location (willow-4j6).
    let (out, ok) = compile_and_run_check_exit("fn main() {\n    panic(\"located\");\n}\n");
    assert!(!ok, "panic must exit non-zero");
    assert!(out.contains("located"), "message present: {out}");
    assert!(
        out.contains(".wi:2:"),
        "debug panic should report source line: {out}"
    );
}

// ── Generic interface declarations (willow-1js.1, slice 1) ───────────────────

#[test]
fn generic_interface_01_declaration_compiles() {
    // A generic interface declares with type params; method sigs may reference
    // them. (Implementing/dispatch on generic interfaces is a later slice.)
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> {
    fn get(self) -> T;
}
interface Conv<A, B> {
    fn run(self, a: A) -> B;
}
fn main() { println(7); }
"#,
    );
    assert!(ok, "generic interface declarations must type-check: {out}");
    assert_eq!(out, "7\n");
}

// ── Generic interfaces: implement, dispatch, conformance (willow-1js.1) ──────

#[test]
fn generic_interface_02_implement_and_dispatch_i64() {
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let b: Box<i64> = new IntBox(99);
    println(b.get());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n");
}

#[test]
fn generic_interface_03_implement_and_dispatch_string() {
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class TextBox implements Box<String> {
    pub init(self, s: String) {
        self.s = s;
    }
    s: String;
    pub fn get(self) -> String { return self.s; }
}
fn main() {
    let b: Box<String> = new TextBox("hi");
    println(b.get());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hi\n");
}

#[test]
fn generic_interface_04_two_type_params() {
    let (out, ok) = compile_and_run(
        r#"
interface Pair<A, B> { fn first(self) -> A; fn second(self) -> B; }
class P implements Pair<i64, String> {
    pub init(self, a: i64, b: String) {
        self.a = a;
        self.b = b;
    }
    a: i64;
    b: String;
    pub fn first(self) -> i64 { return self.a; }
    pub fn second(self) -> String { return self.b; }
}
fn main() {
    let p: Pair<i64, String> = new P(7, "x");
    println(p.first());
    println(p.second());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\nx\n");
}

#[test]
fn generic_interface_05_param_typed_by_type_param() {
    // Method takes an argument whose type is the type parameter.
    let (out, ok) = compile_and_run(
        r#"
interface Sink<T> { fn put(self, v: T) -> T; }
class IntSink implements Sink<i64> {
    pub fn put(self, v: i64) -> i64 { return v + 1; }
}
fn main() {
    let s: Sink<i64> = new IntSink();
    println(s.put(41));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn generic_interface_06_self_return_on_concrete() {
    let (out, ok) = compile_and_run(
        r#"
interface From<E> { fn from(self, e: E) -> Self; }
class W implements From<i64> {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn from(self, e: i64) -> W { return new W(e); }
    pub fn val(self) -> i64 { return self.n; }
}
fn main() {
    let w = new W(0);
    println(w.from(42).val());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn generic_interface_07_passed_to_function() {
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn show(b: Box<i64>) { println(b.get()); }
fn main() { show(new IntBox(5)); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn generic_interface_08_two_instantiations_distinct() {
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
class TextBox implements Box<String> {
    pub init(self, s: String) {
        self.s = s;
    }
    s: String;
    pub fn get(self) -> String { return self.s; }
}
fn main() {
    let a: Box<i64> = new IntBox(1);
    let b: Box<String> = new TextBox("two");
    println(a.get());
    println(b.get());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\ntwo\n");
}

// Negative perspectives ------------------------------------------------------

#[test]
fn generic_interface_neg_01_too_few_type_args() {
    // `Box` requires one type argument (E0422).
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class C implements Box { pub fn get(self) -> i64 { return 1; } }
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_02_too_many_type_args() {
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class C implements Box<i64, i64> { pub fn get(self) -> i64 { return 1; } }
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_03_return_type_mismatch_after_subst() {
    // With T=String, `get` must return String; returning i64 is a mismatch.
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class C implements Box<String> { pub fn get(self) -> i64 { return 1; } }
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_04_param_type_mismatch_after_subst() {
    assert!(expect_compile_error(
        r#"
interface Sink<T> { fn put(self, v: T) -> T; }
class C implements Sink<i64> { pub fn put(self, v: String) -> i64 { return 1; } }
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_05_missing_method() {
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class C implements Box<i64> {}
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_06_unknown_method_on_iface_value() {
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let b: Box<i64> = new IntBox(1);
    b.missing();
}
"#,
    ));
}

#[test]
fn generic_interface_neg_07_wrong_instantiation_not_assignable() {
    // An IntBox implements Box<i64>, not Box<String>.
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let b: Box<String> = new IntBox(1);
}
"#,
    ));
}

// ── `?` automatic error conversion via Into<E> (willow-1ow) ─────────────────

#[test]
fn try_convert_01_err_path_converts() {
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class LowErr implements Into<AppErr> {
    pub n: i64;
    pub fn into(self) -> AppErr { return new AppErr(900 + self.n); }
}
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(5)); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "905\n");
}

#[test]
fn try_convert_02_ok_path_flows_through() {
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class LowErr implements Into<AppErr> {
    pub n: i64;
    pub fn into(self) -> AppErr { return new AppErr(0); }
}
fn low() -> Result<i64, LowErr> { return Result::Ok(11); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v + 1); }
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "12\n");
}

#[test]
fn try_convert_03_exact_match_unaffected() {
    // E1 == E2: no conversion, original error propagates.
    let (out, ok) = compile_and_run(
        r#"
fn low() -> Result<i64, String> { return Result::Err("boom"); }
fn high() -> Result<i64, String> { let v = low()?; return Result::Ok(v); }
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => -1,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-1\n");
}

#[test]
fn try_convert_04_two_question_marks() {
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class LowErr implements Into<AppErr> {
    pub n: i64;
    pub fn into(self) -> AppErr { return new AppErr(self.n); }
}
fn a() -> Result<i64, LowErr> { return Result::Ok(2); }
fn b() -> Result<i64, LowErr> { return Result::Err(new LowErr(77)); }
fn high() -> Result<i64, AppErr> {
    let x = a()?;
    let y = b()?;
    return Result::Ok(x + y);
}
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "77\n");
}

#[test]
fn try_convert_05_no_into_impl_is_error() {
    assert!(expect_compile_error(
        r#"
class AppErr { pub code: i64; }
class LowErr { pub n: i64; }
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(1)); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {}
"#,
    ));
}

#[test]
fn try_convert_06_option_question_unaffected() {
    let (out, ok) = compile_and_run(
        r#"
fn first() -> Option<i64> { return Option::None; }
fn run() -> Option<i64> { let v = first()?; return Option::Some(v + 1); }
fn main() {
    let out = match run() {
        Option::Some(v) => v,
        Option::None => -9,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-9\n");
}

#[test]
fn try_convert_07_into_wrong_target_still_errors() {
    // LowErr implements Into<Other>, not Into<AppErr>: still a mismatch.
    assert!(expect_compile_error(
        r#"
class AppErr { pub code: i64; }
class Other { pub x: i64; }
class LowErr implements Into<Other> {
    pub n: i64;
    pub fn into(self) -> Other { return new Other(0); }
}
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(1)); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {}
"#,
    ));
}

#[test]
fn try_convert_08_payload_data_preserved() {
    // The converted error carries data computed from the source error.
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class LowErr implements Into<AppErr> {
    pub n: i64;
    pub fn into(self) -> AppErr { return new AppErr(self.n * 10); }
}
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(6)); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "60\n");
}

#[test]
fn try_convert_08b_gc_managed_err_payload_rooted_during_into() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class AppErr { pub msg: String; }
class LowErr implements Into<AppErr> {
    pub msg: String;
    pub fn into(self) -> AppErr {
        let prefix = "converted: ";
        gc_collect();
        return new AppErr(prefix + self.msg);
    }
}
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr("payload")); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {
    let out = match high() {
        Result::Ok(v) => "ok",
        Result::Err(e) => e.msg,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "converted: payload\n");
}

#[test]
fn try_convert_09_chained_three_levels() {
    // Conversion at each ? boundary up a three-level call chain.
    let (out, ok) = compile_and_run(
        r#"
class E1 implements Into<E2> {
    pub n: i64;
    pub fn into(self) -> E2 { return new E2(self.n + 1); }
}
class E2 implements Into<E3> {
    pub n: i64;
    pub fn into(self) -> E3 { return new E3(self.n + 1); }
}
class E3 { pub n: i64; }
fn a() -> Result<i64, E1> { return Result::Err(new E1(0)); }
fn b() -> Result<i64, E2> { let v = a()?; return Result::Ok(v); }
fn c() -> Result<i64, E3> { let v = b()?; return Result::Ok(v); }
fn main() {
    let out = match c() {
        Result::Ok(v) => v,
        Result::Err(e) => e.n,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    // E1{0} -> E2{1} at b's ?, then E2{1} -> E3{2} at c's ?.
    assert_eq!(out, "2\n");
}

#[test]
fn try_convert_10_two_source_types_one_target() {
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class IoErr implements Into<AppErr> {
    pub fn into(self) -> AppErr { return new AppErr(1); }
}
class FmtErr implements Into<AppErr> {
    pub fn into(self) -> AppErr { return new AppErr(2); }
}
fn io(fail: bool) -> Result<i64, IoErr> {
    if fail { return Result::Err(new IoErr()); }
    return Result::Ok(10);
}
fn fmt() -> Result<i64, FmtErr> { return Result::Err(new FmtErr()); }
fn high() -> Result<i64, AppErr> {
    let a = io(false)?;
    let b = fmt()?;
    return Result::Ok(a + b);
}
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

// ── Debug call-chain stack traces on panic (willow-992h) ─────────────────────

#[test]
fn callchain_01_nested_panic_prints_ordered_chain() {
    // deeper() <- helper() <- main(): the panic prints the active call chain,
    // most recent call first, with the call-site file:line:col of each frame.
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn deeper() {
    panic("boom");
}
fn helper() {
    deeper();
}
fn main() {
    helper();
}
"#,
    );
    assert!(!ok, "program should abort on panic");
    assert!(
        out.contains("runtime panic: boom"),
        "missing panic line: {out}"
    );
    assert!(
        out.contains("call stack (most recent call first):"),
        "missing call stack header: {out}"
    );
    // Frame 0 is the innermost call (deeper), frame 1 is helper.
    let zero = out.find("0: deeper").expect(&format!("no frame 0: {out}"));
    let one = out.find("1: helper").expect(&format!("no frame 1: {out}"));
    assert!(zero < one, "frames out of order: {out}");
    // Each frame records its call site, not the callee body.
    assert!(
        out.contains("0: deeper at "),
        "frame 0 missing location: {out}"
    );
    assert!(
        out.contains("1: helper at "),
        "frame 1 missing location: {out}"
    );
}

#[test]
fn callchain_02_direct_panic_in_main_has_no_chain() {
    // main is the entry (not called via the instrumented path), so a panic
    // directly in main prints no call-stack section.
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() {
    panic("top");
}
"#,
    );
    assert!(!ok);
    assert!(out.contains("runtime panic: top"), "{out}");
    assert!(
        !out.contains("call stack"),
        "main-only panic should have no chain: {out}"
    );
}

#[test]
fn callchain_03_release_build_omits_chain() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_callchain_rel_{}.wi", id));
    let bin_path = temp_path(format!("willow_callchain_rel_{}", id));
    fs::write(
        &src_path,
        "fn inner() { panic(\"x\"); }\nfn main() { inner(); }\n",
    )
    .unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");
    assert!(status.success(), "release build failed");

    let out = Command::new(&bin_path).output().expect("run failed");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(combined.contains("runtime panic: x"), "{combined}");
    assert!(
        !combined.contains("call stack"),
        "release should omit call chain: {combined}"
    );
}

#[test]
fn callchain_04_three_levels() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn c() { panic("deep"); }
fn b() { c(); }
fn a() { b(); }
fn main() { a(); }
"#,
    );
    assert!(!ok);
    let f0 = out.find("0: c").expect(&format!("{out}"));
    let f1 = out.find("1: b").expect(&format!("{out}"));
    let f2 = out.find("2: a").expect(&format!("{out}"));
    assert!(f0 < f1 && f1 < f2, "chain order wrong: {out}");
}

#[test]
fn callchain_05_method_call_in_chain() {
    // A panic inside a class method shows the method frame above its caller
    // (willow-phx3).
    let (out, ok) = compile_and_run_check_exit(
        r#"
class Worker {
    pub fn run(self) {
        panic("worker failed");
    }
}
fn helper(w: Worker) {
    w.run();
}
fn main() {
    let w = new Worker();
    helper(w);
}
"#,
    );
    assert!(!ok);
    assert!(out.contains("runtime panic: worker failed"), "{out}");
    let run = out
        .find("0: run")
        .expect(&format!("no method frame: {out}"));
    let helper = out
        .find("1: helper")
        .expect(&format!("no caller frame: {out}"));
    assert!(run < helper, "method frame must be innermost: {out}");
}

#[test]
fn async_frame_shadowed_locals_get_distinct_slots() {
    // An outer GC-managed local and a nested shadowed local of the SAME name,
    // both live across awaits, must occupy distinct async-frame slots — the
    // inner write must not clobber the outer (willow-lpn.11). Run under GC
    // stress so a mis-traced/aliased slot is caught.
    let src = r#"
async fn work() -> String {
    let s = "outer";
    await sleep(1);
    if s == "outer" {
        let s = "inner";
        await sleep(1);
        println(s);
    }
    await sleep(1);
    return s;
}

async fn main() {
    let r = await work();
    println(r);
}
"#;
    let (out, ok) = compile_and_run_gc_stress(src);
    assert!(ok, "async shadowing program must run: {out}");
    assert_eq!(
        out, "inner\nouter\n",
        "outer local was clobbered by inner: {out}"
    );
}

#[test]
fn generic_interface_neg_08_two_instantiations_unsatisfiable_rejected() {
    // A class MAY implement two instantiations of the same generic interface
    // (willow-1js.6), but only when one method body can satisfy every
    // instantiation. Here `get(self) -> T` cannot return both `i64` and
    // `String`, so conformance rejects it (E0417), not the duplicate check.
    assert!(expect_compile_error(
        r#"
interface Container<T> { fn get(self) -> T; }
class C implements Container<i64>, Container<String> {
    pub fn get(self) -> i64 { return 1; }
}
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_09_phantom_two_instantiations_allowed() {
    // When the interface's type parameter appears in no method signature
    // (a phantom/marker parameter), a class can implement several
    // instantiations at once; they share one identical vtable (willow-1js.6).
    let (out, ok) = compile_and_run(
        r#"
interface Tagged<T> { fn tag_name(self) -> String; }
class Item implements Tagged<i64>, Tagged<String> {
    pub fn tag_name(self) -> String { return "item"; }
}
fn use_int(t: Tagged<i64>) -> String { return t.tag_name(); }
fn use_str(t: Tagged<String>) -> String { return t.tag_name(); }
fn main() {
    let it = new Item();
    let a: Tagged<i64> = it;
    let b: Tagged<String> = it;
    println(use_int(a));
    println(use_str(b));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "item\nitem\n");
}

#[test]
fn generic_interface_10_exact_duplicate_instantiation_rejected() {
    // The same instantiation listed twice is still a duplicate (E0414).
    assert!(expect_compile_error(
        r#"
interface Tagged<T> { fn tag_name(self) -> String; }
class Item implements Tagged<i64>, Tagged<i64> {
    pub fn tag_name(self) -> String { return "item"; }
}
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_11_phantom_three_instantiations_allowed() {
    // More than two instantiations of a phantom-parameter interface.
    let (out, ok) = compile_and_run(
        r#"
interface Marker<T> { fn kind(self) -> i64; }
class Node implements Marker<i64>, Marker<String>, Marker<bool> {
    pub fn kind(self) -> i64 { return 7; }
}
fn main() {
    let n: Marker<bool> = new Node();
    println(n.kind());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

// ── Default interface methods (willow-1js.3) ─────────────────────────────────

#[test]
fn default_method_01_used_when_not_overridden() {
    let (out, ok) = compile_and_run(
        r#"
interface Greeter {
    fn name(self) -> String;
    fn greet(self) -> String { return "Hi " + self.name(); }
}
class Dog implements Greeter {
    pub fn name(self) -> String { return "Rex"; }
}
fn main() { println(new Dog().greet()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Hi Rex\n");
}

#[test]
fn default_method_02_override_wins() {
    let (out, ok) = compile_and_run(
        r#"
interface Greeter {
    fn name(self) -> String;
    fn greet(self) -> String { return "Hi " + self.name(); }
}
class Cat implements Greeter {
    pub fn name(self) -> String { return "Tom"; }
    pub fn greet(self) -> String { return "Meow " + self.name(); }
}
fn main() { println(new Cat().greet()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Meow Tom\n");
}

#[test]
fn default_method_03_dispatch_through_interface() {
    let (out, ok) = compile_and_run(
        r#"
interface Greeter {
    fn name(self) -> String;
    fn greet(self) -> String { return "Hi " + self.name(); }
}
class Dog implements Greeter { pub fn name(self) -> String { return "Rex"; } }
fn run(g: Greeter) { println(g.greet()); }
fn main() { run(new Dog()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Hi Rex\n");
}

#[test]
fn default_method_04_default_calls_default() {
    let (out, ok) = compile_and_run(
        r#"
interface Calc {
    fn base(self) -> i64;
    fn doubled(self) -> i64 { return self.base() * 2; }
    fn plus(self, n: i64) -> i64 { return self.doubled() + n; }
}
class Num implements Calc { pub fn base(self) -> i64 { return 5; } }
fn main() {
    let n = new Num();
    println(n.doubled());
    println(n.plus(3));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n13\n");
}

#[test]
fn default_method_05_override_seen_by_other_default() {
    // shout() is a default that calls greet(); when greet() is overridden, the
    // default shout() must call the override (dynamic self-dispatch).
    let (out, ok) = compile_and_run(
        r#"
interface Greeter {
    fn name(self) -> String;
    fn greet(self) -> String { return "Hi " + self.name(); }
    fn shout(self) -> String { return self.greet() + "!"; }
}
class Robot implements Greeter {
    pub fn name(self) -> String { return "R2"; }
    pub fn greet(self) -> String { return "BEEP " + self.name(); }
}
fn main() { println(new Robot().shout()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "BEEP R2!\n");
}

#[test]
fn default_method_06_required_method_still_enforced() {
    // A non-default (required) method must still be implemented.
    assert!(expect_compile_error(
        r#"
interface I {
    fn req(self) -> i64;
    fn opt(self) -> i64 { return 1; }
}
class C implements I {}
fn main() {}
"#,
    ));
}

#[test]
fn default_method_07_no_self_default_rejected() {
    // A default body requires a `self` receiver (E0420).
    assert!(expect_compile_error(
        r#"
interface I { fn f() { return; } }
fn main() {}
"#,
    ));
}

// ── Interface inheritance (willow-1js.2) ─────────────────────────────────────

#[test]
fn iface_inherit_01_class_usable_as_sub_and_super() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
interface Pet extends Animal { fn owner(self) -> String; }
class Dog implements Pet {
    pub fn name(self) -> String { return "Rex"; }
    pub fn owner(self) -> String { return "Sam"; }
}
fn as_animal(a: Animal) { println(a.name()); }
fn as_pet(p: Pet) { println(p.name() + "/" + p.owner()); }
fn main() {
    let d = new Dog();
    as_pet(d);
    as_animal(d);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Rex/Sam\nRex\n");
}

#[test]
fn iface_inherit_02_sub_interface_value_as_super() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
interface Pet extends Animal { fn owner(self) -> String; }
class Dog implements Pet {
    pub fn name(self) -> String { return "Rex"; }
    pub fn owner(self) -> String { return "Sam"; }
}
fn as_animal(a: Animal) { println(a.name()); }
fn main() {
    let p: Pet = new Dog();
    as_animal(p);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Rex\n");
}

#[test]
fn iface_inherit_03_missing_inherited_required_method_errors() {
    assert!(expect_compile_error(
        r#"
interface Animal { fn name(self) -> String; }
interface Pet extends Animal { fn owner(self) -> String; }
class Bad implements Pet { pub fn owner(self) -> String { return "x"; } }
fn main() {}
"#,
    ));
}

#[test]
fn iface_inherit_04_inherited_default_method() {
    let (out, ok) = compile_and_run(
        r#"
interface Named {
    fn name(self) -> String;
    fn label(self) -> String { return "name=" + self.name(); }
}
interface Pet extends Named { fn owner(self) -> String; }
class Dog implements Pet {
    pub fn name(self) -> String { return "Rex"; }
    pub fn owner(self) -> String { return "Sam"; }
}
fn main() {
    let p: Pet = new Dog();
    println(p.label());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "name=Rex\n");
}

#[test]
fn iface_inherit_05_transitive_three_levels() {
    let (out, ok) = compile_and_run(
        r#"
interface A { fn a(self) -> i64; }
interface B extends A { fn b(self) -> i64; }
interface C extends B { fn c(self) -> i64; }
class Impl implements C {
    pub fn a(self) -> i64 { return 1; }
    pub fn b(self) -> i64 { return 2; }
    pub fn c(self) -> i64 { return 3; }
}
fn sum_a(x: A) -> i64 { return x.a(); }
fn main() {
    let v: C = new Impl();
    println(sum_a(v) + v.b() + v.c());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// ── Interface -> concrete downcast via match (willow-1js.4) ──────────────────

#[test]
fn downcast_01_matches_concrete_and_calls_method() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal {
    pub fn name(self) -> String { return "Rex"; }
    pub fn bark(self) -> String { return "woof"; }
}
class Cat implements Animal {
    pub fn name(self) -> String { return "Tom"; }
    pub fn meow(self) -> String { return "meow"; }
}
fn sound(a: Animal) -> String {
    return match a {
        Dog(d) => d.bark(),
        Cat(c) => c.meow(),
        _ => "?",
    };
}
fn main() {
    println(sound(new Dog()));
    println(sound(new Cat()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn downcast_02_wildcard_handles_other_classes() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "Rex"; } pub fn bark(self) -> String { return "woof"; } }
class Fish implements Animal { pub fn name(self) -> String { return "Nemo"; } }
fn sound(a: Animal) -> String {
    return match a {
        Dog(d) => d.bark(),
        _ => a.name(),
    };
}
fn main() {
    println(sound(new Dog()));
    println(sound(new Fish()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "woof\nNemo\n");
}

#[test]
fn downcast_03_underscore_binding_no_bind() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "Rex"; } }
class Cat implements Animal { pub fn name(self) -> String { return "Tom"; } }
fn kind(a: Animal) -> String {
    return match a {
        Dog(_) => "dog",
        Cat(_) => "cat",
        _ => "other",
    };
}
fn main() {
    println(kind(new Dog()));
    println(kind(new Cat()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "dog\ncat\n");
}

#[test]
fn downcast_04_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "Rex"; } pub fn bark(self) -> String { return "woof " + self.name(); } }
class Cat implements Animal { pub fn name(self) -> String { return "Tom"; } }
fn sound(a: Animal) -> String {
    return match a {
        Dog(d) => d.bark(),
        _ => a.name(),
    };
}
fn main() {
    println(sound(new Dog()));
    println(sound(new Cat()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "woof Rex\nTom\n");
}

#[test]
fn downcast_04b_debug_build_embeds_nil_guard_contexts() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_downcast_guard_{}.wi", id));
    let bin_path = temp_path(format!("willow_downcast_guard_{}", id));
    let source = r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "Rex"; } }
class Cat implements Animal { pub fn name(self) -> String { return "Tom"; } }
fn kind(a: Animal) -> String {
    return match a {
        Dog(_) => "dog",
        _ => "other",
    };
}
fn main() { println(kind(new Cat())); }
"#;
    std::fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = std::process::Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to compile");
    assert!(output.status.success(), "should compile");

    let binary = std::fs::read(&bin_path).expect("binary should exist");
    let content = String::from_utf8_lossy(&binary);

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(format!("{bin_path}.wsmap"));

    assert!(content.contains("interface downcast box"));
    assert!(content.contains("interface downcast object"));
}

#[test]
fn downcast_neg_01_non_interface_scrutinee() {
    assert!(expect_compile_error(
        r#"
class Dog { pub fn bark(self) -> String { return "w"; } }
fn main() {
    let d = new Dog();
    let s = match d { Dog(x) => x.bark(), _ => "no" };
    println(s);
}
"#,
    ));
}

#[test]
fn downcast_neg_02_class_not_implementing_interface() {
    assert!(expect_compile_error(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "R"; } }
class Tree { pub fn h(self) -> i64 { return 1; } }
fn f(a: Animal) -> i64 { return match a { Tree(t) => t.h(), _ => 0 }; }
fn main() {}
"#,
    ));
}

// ── Interface inheritance validation (willow-1js.8) ──────────────────────────

#[test]
fn iface_inherit_neg_01_cycle_rejected() {
    assert!(expect_compile_error(
        r#"
interface A extends B { fn a(self) -> i64; }
interface B extends A { fn b(self) -> i64; }
fn main() {}
"#,
    ));
}

#[test]
fn iface_inherit_02_multiple_supers_allowed() {
    let (out, ok) = compile_and_run(
        r#"
interface A { fn a(self) -> i64; }
interface B { fn b(self) -> i64; }
interface C extends A, B { fn c(self) -> i64; }
class Impl implements C {
    pub fn a(self) -> i64 { return 10; }
    pub fn b(self) -> i64 { return 20; }
    pub fn c(self) -> i64 { return 30; }
}
fn main() {
    let c: C = new Impl();
    println(c.a() + c.b() + c.c());
}
"#,
    );
    assert!(ok, "multiple super-interface inheritance should compile");
    assert_eq!(out, "60\n");
}

#[test]
fn iface_inherit_neg_03_extends_class_rejected() {
    assert!(expect_compile_error(
        r#"
class Foo { pub fn f(self) -> i64 { return 1; } }
interface Bad extends Foo { fn g(self) -> i64; }
fn main() {}
"#,
    ));
}

#[test]
fn iface_inherit_neg_04_extends_unknown_rejected() {
    assert!(expect_compile_error(
        r#"
interface Bad extends Nope { fn g(self) -> i64; }
fn main() {}
"#,
    ));
}

#[test]
fn downcast_05_generic_interface_scrutinee() {
    // Downcast works when the scrutinee is a generic interface instantiation
    // (willow-1js.9).
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    pub fn get(self) -> i64 { return 7; }
    pub fn extra(self) -> i64 { return 99; }
}
class OtherBox implements Box<i64> { pub fn get(self) -> i64 { return 1; } }
fn probe(b: Box<i64>) -> i64 {
    return match b {
        IntBox(x) => x.extra(),
        _ => b.get(),
    };
}
fn main() {
    println(probe(new IntBox()));
    println(probe(new OtherBox()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n1\n");
}

// ── Subclass usable as a base-declared interface (willow-2s4i) ───────────────

#[test]
fn subclass_iface_01_used_as_base_interface() {
    // Puppy extends Dog (which implements Animal); a Puppy is an Animal even
    // though Puppy does not re-declare `implements Animal`.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
interface Animal { fn name(self) -> String; }
open class Dog implements Animal { pub open fn name(self) -> String { return "dog"; } }
class Puppy extends Dog { pub override fn name(self) -> String { return "puppy"; } }
fn describe(a: Animal) { println(a.name()); }
fn main() {
    describe(new Dog());
    describe(new Puppy());
}
"#,
    );
    assert!(ok, "subclass must be usable as the base's interface: {out}");
    assert_eq!(out, "dog\npuppy\n");
}

#[test]
fn subclass_iface_02_inherits_method_no_override() {
    // The subclass inherits the base's interface method (no override).
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn legs(self) -> i64; }
open class Dog implements Animal { pub fn legs(self) -> i64 { return 4; } }
class Puppy extends Dog {}
fn count(a: Animal) -> i64 { return a.legs(); }
fn main() { println(count(new Puppy())); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

#[test]
fn subclass_iface_03_two_levels() {
    // Grandchild is usable as the interface declared two levels up.
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
open class Dog implements Animal { pub open fn name(self) -> String { return "dog"; } }
open class Puppy extends Dog { pub open override fn name(self) -> String { return "puppy"; } }
class Teacup extends Puppy { pub override fn name(self) -> String { return "teacup"; } }
fn describe(a: Animal) { println(a.name()); }
fn main() { describe(new Teacup()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "teacup\n");
}

// ── Virtual dispatch for overridden methods (willow-ftk) ─────────────────────

#[test]
fn virtual_dispatch_01_override_via_base_ref() {
    let (out, ok) = compile_and_run(
        r#"
open class Animal { pub open fn sound(self) -> String { return "..."; } }
class Dog extends Animal { pub override fn sound(self) -> String { return "woof"; } }
class Cat extends Animal { pub override fn sound(self) -> String { return "meow"; } }
fn speak(a: Animal) { println(a.sound()); }
fn main() { speak(new Dog()); speak(new Cat()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn virtual_dispatch_02_base_method_calls_overridden_self() {
    // An inherited base method that calls self.m() dispatches to the override.
    let (out, ok) = compile_and_run(
        r#"
open class Animal {
    pub open fn sound(self) -> String { return "..."; }
    pub fn describe(self) -> String { return "I say " + self.sound(); }
}
class Dog extends Animal { pub override fn sound(self) -> String { return "woof"; } }
fn main() { println(new Dog().describe()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "I say woof\n");
}

#[test]
fn virtual_dispatch_03_inherited_non_override_dispatches_to_base() {
    // A subclass that does NOT override must dispatch to the inherited base
    // implementation (regression for the fall-through bug, willow-ftk).
    let (out, ok) = compile_and_run(
        r#"
open class Animal { pub open fn sound(self) -> String { return "base"; } }
class Mute extends Animal {}
fn speak(a: Animal) { println(a.sound()); }
fn main() { speak(new Mute()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "base\n");
}

#[test]
fn virtual_dispatch_04_three_levels_mixed_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;
open class A {
    pub open fn kind(self) -> String { return "A"; }
    pub fn tag(self) -> String { return "[" + self.kind() + "]"; }
}
open class B extends A { pub open override fn kind(self) -> String { return "B"; } }
class C extends B { pub override fn kind(self) -> String { return "C"; } }
class D extends A {}
fn main() {
    let xs: Array<A> = [new A(), new B(), new C(), new D()];
    let mut i = 0;
    while i < xs.len() {
        println(xs[i].tag());
        i = i + 1;
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[A]\n[B]\n[C]\n[A]\n");
}

// ── ? error conversion: virtual Into dispatch on subclassed errors (bpk6) ────

#[test]
fn try_convert_11_subclassed_error_uses_override() {
    // A Result<_, BaseErr> holding a SpecificErr (override of into) must convert
    // via the override when propagated with `?` (willow-bpk6).
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
open class BaseErr implements Into<AppErr> {
    pub open fn into(self) -> AppErr { return new AppErr(1); }
}
class SpecificErr extends BaseErr {
    pub override fn into(self) -> AppErr { return new AppErr(99); }
}
fn fails() -> Result<i64, BaseErr> {
    let e: BaseErr = new SpecificErr();
    return Result::Err(e);
}
fn run() -> Result<i64, AppErr> { let v = fails()?; return Result::Ok(v); }
fn main() {
    let out = match run() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n");
}

#[test]
fn try_convert_12_base_error_uses_base_into() {
    // The same hierarchy: a plain BaseErr converts via BaseErr::into.
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
open class BaseErr implements Into<AppErr> {
    pub open fn into(self) -> AppErr { return new AppErr(1); }
}
class SpecificErr extends BaseErr {
    pub override fn into(self) -> AppErr { return new AppErr(99); }
}
fn fails() -> Result<i64, BaseErr> { return Result::Err(new BaseErr()); }
fn run() -> Result<i64, AppErr> { let v = fails()?; return Result::Ok(v); }
fn main() {
    let out = match run() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn subclass_iface_04_inherits_generic_interface_with_args() {
    // A subclass inherits a generic interface (Into<AppErr>) from its base with
    // type args preserved (regression for the name-only propagation bug).
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
open class BaseErr implements Into<AppErr> {
    pub open fn into(self) -> AppErr { return new AppErr(7); }
}
class SubErr extends BaseErr {}
fn convert(e: Into<AppErr>) -> i64 { return e.into().code; }
fn main() { println(convert(new SubErr())); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

// ── Cooperative async suspension (willow-lpn.5.3 Stage 2) ────────────────────

#[test]
fn coop_async_01_main_suspends_at_sleep() {
    // An eligible `async fn main` lowers to a suspending poll-fn state machine
    // driven by the scheduler; output is produced across the await points.
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    println(1);
    await sleep(1);
    println(2);
    await sleep(1);
    println(3);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

#[test]
fn coop_async_02_no_await_before_first_output() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    await sleep(1);
    println(42);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn coop_async_03_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    println(1);
    await sleep(1);
    println(2);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn coop_async_04_gc_locals_across_awaits() {
    // GC-managed locals declared before an await and used after must survive
    // suspension (frame-backed). Run under GC stress (willow-lpn.5.3 slice 3).
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    let s = "hello";
    await sleep(1);
    println(s);
    let t = s + " world";
    await sleep(1);
    println(t);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hello\nhello world\n");
}

#[test]
fn coop_async_05_non_gc_locals_across_awaits() {
    // i64/scalar locals across awaits are frame-backed too (not just GC), and
    // are not GC-traced (willow-lpn.5.3 slice 3b). GC-stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    let n = 10;
    let s = "v=";
    await sleep(1);
    let m = n + 5;
    println(s);
    println(m);
    await sleep(1);
    println(n);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "v=\n15\n10\n");
}

#[test]
fn coop_async_06_await_cooperative_leaf() {
    // A no-param leaf async fn (sleep + return) compiles to a cooperative
    // constructor + poll fn; `await f()` block-runs the scheduler and reads the
    // result (willow-lpn.5.3 slice 4). GC-stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn wait_value() -> i64 {
    await sleep(1);
    return 42;
}
async fn compute() -> i64 {
    await sleep(1);
    return 7;
}
async fn main() {
    let x = await wait_value();
    println(x);
    let y = await compute();
    println(y + 1);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n8\n");
}

#[test]
fn coop_async_06b_eager_await_roots_leaf_frame_until_result_load() {
    // `println(await f())` is intentionally not eligible for the cooperative
    // awaiter lowering, so it exercises the eager emit_await() path. That path
    // must keep the completed leaf frame rooted while willow_sched_run() removes
    // the task runtime root and before frame[RESULT] is loaded.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn make_text() -> String {
    await sleep(1);
    return "root" + "ed";
}
async fn make_number() -> i64 {
    await sleep(1);
    return 42;
}
async fn main() {
    println(await make_text());
    println(await make_number());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "rooted\n42\n");
}

#[test]
fn coop_async_06c_eager_await_survives_await_stress() {
    let (out, ok) = compile_and_run_gc_stress_mode(
        r#"
async fn make_text() -> String {
    await sleep(1);
    return "await" + "-stress";
}
async fn main() {
    println(await make_text());
}
"#,
        "await",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "await-stress\n");
}

#[test]
fn coop_async_07_cooperative_leaf_with_params() {
    // A leaf async fn with by-value params (GC + scalar) compiles to a
    // cooperative constructor that stores args into frame slots; the poll fn
    // reads them back across the suspension (willow-lpn.5.3 slice 4b). GC-stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn greet(name: String, n: i64) -> String {
    await sleep(1);
    return "hi " + name;
}
async fn add(a: i64, b: i64) -> i64 {
    await sleep(1);
    return a + b;
}
async fn main() {
    let g = await greet("willow", 3);
    println(g);
    let s = await add(40, 2);
    println(s);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hi willow\n42\n");
}

#[test]
fn coop_async_08_cooperative_leaf_with_locals() {
    // A cooperative leaf may declare locals (GC + scalar) that survive its own
    // suspensions, frame-backed after the param slots (willow-lpn.5.3 4c).
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn calc(base: i64) -> i64 {
    let a = base + 1;
    let label = "result";
    await sleep(1);
    let b = a * 2;
    await sleep(1);
    println(label);
    return b + base;
}
async fn main() {
    let r = await calc(10);
    println(r);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "result\n32\n");
}

#[test]
fn coop_async_09_await_inside_if_and_while_in_main() {
    // Slice 5: structured control flow in the cooperative main poll fn, including
    // a loop back-edge and branch-local suspend points.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    let mut i = 0;
    while i < 3 {
        if i == 1 {
            await sleep(1);
            println(10);
        } else {
            await sleep(1);
            println(i);
        }
        i = i + 1;
    }
    println(99);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n10\n2\n99\n");
}

#[test]
fn coop_async_10_await_inside_leaf_if_else_returns() {
    // Slice 5 regression: both branches can suspend and then return from a
    // cooperative leaf poll fn.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn pick(flag: bool) -> i64 {
    if flag {
        await sleep(1);
        return 10;
    } else {
        await sleep(1);
        await sleep(1);
        return 20;
    }
}
async fn main() {
    let a = await pick(true);
    println(a);
    let b = await pick(false);
    println(b);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n20\n");
}

#[test]
fn coop_async_11_await_inside_for_loop_in_main() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn main() {
    let xs: Array<i64> = [1, 2, 3];
    for x in xs {
        await sleep(1);
        println(x);
    }
    println(99);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n99\n");
}

#[test]
fn coop_async_12_await_inside_for_loop_in_leaf() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn sum(values: Array<i64>) -> i64 {
    let mut total = 0;
    for value in values {
        await sleep(1);
        total = total + value;
    }
    return total;
}

async fn main() {
    let values: Array<i64> = [4, 5, 6];
    let total = await sum(values);
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "15\n");
}
