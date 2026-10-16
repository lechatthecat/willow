use super::*;

const EXAMPLE: &str = include_str!("../../../example/async_string_reference.wi");

// Both dispatch paths can select an async body despite a synchronous signature.
fn dispatch_reference_source(interface: bool, mode: &str, ty: &str, body: &str) -> String {
    let declarations = if interface {
        format!("interface Reading extends Sync {{ fn read(self, text: {mode} {ty}) -> Task<String>; }}
            open class Parent {{ pub async fn read(self, text: {mode} {ty}) -> String {{ await sleep(1); return \"done\"; }} }}
            class Reader extends Parent implements Reading {{}}")
    } else {
        format!("open class Reading {{ pub open fn read(self, text: {mode} {ty}) -> Task<String> {{ return value(); }} }}
            class Reader extends Reading {{ pub override async fn read(self, text: {mode} {ty}) -> String {{ await sleep(1); return \"done\"; }} }}")
    };
    format!(
        "fn inspect(x: &String) {{}} fn replace(x: &mut String) {{ x = \"changed\"; }} fn consume(t: Task<String>) {{}} async fn value() -> String {{ return \"done\"; }} {declarations}
        async fn main() {{ let reader: Reading = new Reader(); {body} }}"
    )
}

fn importer_dispatch_project(
    interface: bool,
    mode: &str,
    ty: &str,
    body: &str,
) -> (String, String) {
    let declaration = if interface {
        format!(
            "pub interface Reading extends Sync {{ fn read(self, text: {mode} {ty}) -> Task<String>; }}"
        )
    } else {
        format!(
            "pub open class Reading {{ pub open fn read(self, text: {mode} {ty}) -> Task<String> {{ return value(); }} }} async fn value() -> String {{ return \"independent\"; }}"
        )
    };
    let api = format!("module api; {declaration} fn inspect(x: &String) {{}} fn replace(x: &mut String) {{ x = \"changed\"; }}
        pub async fn use_reader(reader: Reading) {{ {body} }}");
    let relation = if interface { "implements" } else { "extends" };
    let modifier = if interface { "" } else { "override" };
    let result = if ty == "String" { "text" } else { "\"done\"" };
    let main = format!("import api::Reading; import api::use_reader;
        class Reader {relation} Reading {{ pub {modifier} async fn read(self, text: {mode} {ty}) -> String {{ await sleep(1); gc_minor_collect(); gc_collect(); return {result}; }} }}
        async fn main() {{ await use_reader(new Reader()); }}");
    (api, main)
}

#[test]
fn async_gc_reference_importer_dispatch_diagnostics() {
    for interface in [false, true] {
        for (body, code) in [
            ("let task = reader.read(&text);", "E1708"),
            ("reader.read(&text);", "E1708"),
            (
                "let task = reader.read(&text); inspect(&text); await task;",
                "E1706",
            ),
            (
                "let task = reader.read(&text); replace(&text); await task;",
                "E1706",
            ),
            (
                "let task = reader.read(&text); let other = reader.read(&text); await task; await other;",
                "E1706",
            ),
        ] {
            let (api, main) = importer_dispatch_project(
                interface,
                "&mut",
                "String",
                &format!("let mut text = \"local\" + \" string\"; {body}"),
            );
            let out = compile_temp_project_error_stderr(
                &[("api.wi", &api), ("main.wi", &main)],
                "main.wi",
            );
            assert!(out.contains(code), "interface={interface} {body}: {out}");
        }
        for mode in ["&", "&mut", ""] {
            let arg = if mode.is_empty() { "text" } else { "&text" };
            let (api, main) = importer_dispatch_project(
                interface,
                mode,
                "Array<String>",
                &format!("let mut text: Array<String> = [\"value\"]; await reader.read({arg});"),
            );
            let out = compile_temp_project_error_stderr(
                &[("api.wi", &api), ("main.wi", &main)],
                "main.wi",
            );
            assert!(
                out.contains("E2402")
                    && out.contains("not `Sync`")
                    && out.contains("to an async call")
                    && out.contains("api.wi"),
                "interface={interface} mode={mode}: {out}"
            );
        }
    }
}

#[test]
fn async_gc_reference_importer_dispatch_relocation() {
    for interface in [false, true] {
        for mode in ["&", "&mut"] {
            let (api, main) = importer_dispatch_project(
                interface,
                mode,
                "String",
                "let mut text = \"borrowed\" + \" string\"; let task = reader.read(&text); let result = await task; println(result);",
            );
            let (out, ok) = compile_temp_project_with_env_and_run_under(
                &[("api.wi", &api), ("main.wi", &main)],
                "main.wi",
                &[],
                &[("WILLOW_GC_STRESS", "relocate")],
            );
            assert!(ok, "interface={interface} mode={mode}: {out}");
            assert_eq!(
                out, "borrowed string\n",
                "interface={interface} mode={mode}"
            );
        }
    }
}

#[test]
fn async_gc_reference_dispatch_diagnostics() {
    for interface in [false, true] {
        for (body, code) in [
            ("let task = reader.read(&text);", "E1708"),
            ("reader.read(&text);", "E1708"),
            ("let task = reader.read(&text); consume(task);", "E1708"),
            (
                "let task = reader.read(&text); inspect(&text); await task;",
                "E1706",
            ),
            (
                "let task = reader.read(&text); replace(&text); await task;",
                "E1706",
            ),
            (
                "let task = reader.read(&text); let other = reader.read(&text); await task; await other;",
                "E1706",
            ),
        ] {
            let source = dispatch_reference_source(
                interface,
                "&mut",
                "String",
                &format!("let mut text = \"borrowed\" + \" string\"; {body}"),
            );
            let out = compile_error_stderr(&source);
            assert!(out.contains(code), "interface={interface} {body}: {out}");
        }
        for mode in ["&", "&mut"] {
            let source = dispatch_reference_source(
                interface,
                mode,
                "String",
                "let mut text = \"local\"; return reader.read(&text);",
            )
            .replace("async fn main()", "fn escape() -> Task<String>")
                + " fn main() {}";
            let out = compile_error_stderr(&source);
            assert!(
                out.contains("E1708"),
                "interface={interface} mode={mode} return escape: {out}"
            );
        }
        for (bound, code) in [("", "E2404"), ("extends Send", "E2405")] {
            let source = format!(
                "interface Payload {bound} {{}} class Data implements Payload {{}} {}",
                dispatch_reference_source(
                    interface,
                    "",
                    "Payload",
                    "let text: Payload = new Data(); await reader.read(text);"
                )
            );
            let out = compile_error_stderr(&source);
            assert!(
                out.contains(code),
                "interface={interface} bound={bound}: {out}"
            );
        }
        for mode in ["&", "&mut", ""] {
            let arg = if mode.is_empty() { "text" } else { "&text" };
            let source = dispatch_reference_source(
                interface,
                mode,
                "Array<String>",
                &format!("let mut text: Array<String> = [\"borrowed\"]; await reader.read({arg});"),
            );
            let out = compile_error_stderr(&source);
            assert!(
                out.contains("E2402") && out.contains("not `Sync`"),
                "interface={interface} mode={mode}: {out}"
            );
        }
    }
}

#[test]
fn async_gc_reference_dispatch_independent_tasks() {
    for interface in [false, true] {
        let source = dispatch_reference_source(
            interface,
            "&mut",
            "String",
            "let mut text = \"local\"; let task = reader.read(&text); inspect(&text); replace(&text); println(text); consume(task);",
        );
        let source = source.replace(
            "async fn read(self, text: &mut String) -> String { await sleep(1); return \"done\"; }",
            "fn read(self, text: &mut String) -> Task<String> { return value(); }",
        );
        let (out, ok) = compile_and_run_gc_stress_mode(&source, "relocate");
        assert!(ok, "interface={interface}: {out}");
        assert_eq!(out, "changed\n");
    }
    let source = r#"
        async fn value() -> String { return "done"; }
        open class Base { pub open fn read(self, text: &mut String) -> Task<String> { return value(); } }
        class Child extends Base {
            pub override async fn read(self, text: &mut String) -> String { return text; }
            pub fn independent(self, text: &mut String) -> Task<String> { return super.read(&text); }
        }
        async fn main() { let child = new Child(); let mut text = "local";
            let task = child.independent(&text); let other = child.independent(&text);
            println(await task); println(await other);
        }
    "#;
    let (out, ok) = compile_and_run_gc_stress_mode(source, "relocate");
    assert!(ok, "{out}");
    assert_eq!(out, "done\ndone\n");
}

#[test]
fn async_gc_reference_method_without_later_caller_read() {
    for mode in ["&", "&mut"] {
        for shape in ["instance", "inherited", "suspending_argument", "static"] {
            let (declaration, receiver, call) = match shape {
                "static" => ("static async fn read(text", "", "Reader::read(&text, 0)"),
                "inherited" => (
                    "async fn read(self, text",
                    "let reader = new Child();",
                    "reader.read(&text, 0)",
                ),
                "suspending_argument" => (
                    "async fn read(self, text",
                    "let reader = new Reader();",
                    "reader.read(&text, await token())",
                ),
                _ => (
                    "async fn read(self, text",
                    "let reader = new Reader();",
                    "reader.read(&text, 0)",
                ),
            };
            let source = format!(
                "open class Reader {{
                pub {declaration}: {mode} String, unused: i64) -> String {{
                    await sleep(1); gc_minor_collect(); gc_collect(); return text;
                }}
            }} class Child extends Reader {{}}
            async fn token() -> i64 {{ await sleep(1); return 0; }}
            async fn main() {{
                {receiver} let mut text = \"borrowed\" + \" string\";
                let task = {call}; let result = await task; println(result);
            }}"
            );
            let (out, ok) = compile_and_run_gc_stress_mode(&source, "relocate");
            assert!(ok, "{mode}/{shape}: {out}");
            assert_eq!(out, "borrowed string\n", "{mode}/{shape}");
        }
    }
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "reader.wi",
                "module reader; pub async fn read(text: &String) -> String { await sleep(1); gc_minor_collect(); gc_collect(); return text; }",
            ),
            (
                "main.wi",
                "import reader; async fn main() { let text = \"module\" + \" string\"; let task = reader::read(&text); let result = await task; println(result); }",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "module string\n");
    let source = r#"
        async fn value(text: String) -> String { return text; }
        open class Base {
            pub open fn read(self, text: &String) -> Task<String> { return value(text); }
        }
        class Reader extends Base {
            pub override async fn read(self, text: &String) -> String {
                await sleep(1); gc_collect(); return text;
            }
        }
        async fn main() {
            let reader: Base = new Reader(); let text = "virtual" + " string";
            let task = reader.read(&text); let result = await task; println(result);
        }
    "#;
    let (out, ok) = compile_and_run_gc_stress_mode(source, "relocate");
    assert!(ok, "{out}");
    assert_eq!(out, "virtual string\n");
    let source = r#"
        interface Reading extends Sync { fn read(self, text: &String) -> Task<String>; }
        open class AsyncParent {
            pub async fn read(self, text: &String) -> String {
                await sleep(1); gc_collect(); return text;
            }
        }
        class Child extends AsyncParent implements Reading {}
        async fn main() {
            let reader: Reading = new Child(); let text = "interface" + " string";
            let task = reader.read(&text); let result = await task; println(result);
        }
    "#;
    let (out, ok) = compile_and_run_gc_stress_mode(source, "relocate");
    assert!(ok, "{out}");
    assert_eq!(out, "interface string\n");
}

#[test]
fn async_gc_reference_example_modes() {
    for mode in ["debug", "release", "minor", "all", "relocate"] {
        let (out, ok) = match mode {
            "debug" => compile_and_run(EXAMPLE),
            "release" => compile_and_run_release(EXAMPLE),
            mode => compile_and_run_gc_stress_mode(EXAMPLE, mode),
        };
        assert!(ok, "{mode}: {out}");
        assert_eq!(
            out, "moved string\nmoved string\nreplacement\nold alias\n",
            "{mode}"
        );
    }
}

// 24 independent storage/path perspectives under relocating GC stress.
#[test]
fn async_gc_reference_paths() {
    for (ty, initial, updated, print) in [
        ("String", "\"old\"", "\"new\" + \" value\"", "x"),
        ("Element", "\"old\"", "\"new\" + \" value\"", "x"),
        (
            "Box",
            "new Box(\"old\")",
            "new Box(\"new\" + \" value\")",
            "x.value",
        ),
    ] {
        let element = ty == "Element";
        let ty = if element { "String" } else { ty };
        for shape in 0..8 {
            let body = match shape {
                0 => "await update(&x);",
                1 => "let t = update(&x); await sleep(1); await t;",
                2 => "await forward(&x);",
                3 => "await sync_forward(&x);",
                4 => "await deferred(&x);",
                5 => "let s = TaskScope::new(); s.add(update(&x)); await s.finish();",
                6 => "let mut i = 0; while i < 3 { await update(&x); i = i + 1; }",
                _ => "await update(&x); await read(&x);",
            };
            let main = if element {
                format!(
                    "let values: Array<String> = [\"old\"]; {} gc_collect(); println(values[0]);",
                    body.replace("&x", "&values[0]")
                )
            } else {
                format!("let mut x: {ty} = {initial}; {body} gc_collect(); println({print});")
            };
            let source = format!("import std::collections::Array;
                class Box {{ pub value: String; }}
                async fn update(x: &mut {ty}) {{ await sleep(1); x = {updated}; gc_collect(); await sleep(1); }}
                async fn forward(x: &mut {ty}) {{ await sleep(1); await update(&x); }}
                fn sync_update(x: &mut {ty}) {{ x = {updated}; gc_collect(); }}
                async fn sync_forward(x: &mut {ty}) {{ await sleep(1); sync_update(&x); }}
                async fn deferred(x: &mut {ty}) {{ defer {{ sync_update(&x); }} await sleep(1); }}
                async fn read(x: &{ty}) {{ await sleep(1); gc_collect(); println({print}); }}
                async fn main() {{ {main} }}");
            let (out, ok) = compile_and_run_gc_stress_mode(&source, "relocate");
            assert!(ok, "{ty}/{shape}/element={element}: {out}");
            assert_eq!(
                out,
                "new value\n".repeat(if shape == 7 { 2 } else { 1 }),
                "{ty}/{shape}"
            );
        }
    }
}

#[test]
fn async_gc_reference_moving_writeback() {
    for collect in ["gc_minor_collect", "gc_collect"] {
        let counter = if collect == "gc_minor_collect" {
            "gc_moved_objects"
        } else {
            "gc_major_collections"
        };
        let source = format!(
            "async fn update(x: &mut String) {{
            await sleep(1); x = \"fresh\" + \" string\";
            let before = {counter}(); {collect}();
            println({counter}() > before); println(x);
            await sleep(1); println(x);
        }} async fn main() {{ let mut x = \"old\"; await update(&x); println(x); }}"
        );
        let project = TestProject::new("gc_ref_writeback", &[("main.wi", &source)]);
        let compiled = project.compile("main.wi");
        assert!(compiled.status.success(), "{compiled:?}");
        // Minor proves movement; the current major collector marks/sweeps without moving.
        let mode = if collect == "gc_minor_collect" {
            ""
        } else {
            "relocate"
        };
        let out = project.run_with_env(&[
            ("WILLOW_GC_STRESS", mode),
            ("WILLOW_WORKERS", "1"),
            ("WILLOW_GC_VERIFY_NO_PIN", "1"),
            ("WILLOW_GC_VERIFY_BARRIER", "1"),
            ("WILLOW_GC_VERIFY_REGIONS", "1"),
        ]);
        assert!(out.status.success(), "{collect}: {out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "true\nfresh string\nfresh string\nfresh string\n",
            "{collect}"
        );
    }
}

#[test]
fn async_gc_reference_heap_places_and_aliases() {
    let source = r#"import std::collections::Array;
        class Box { pub value: String; }
        async fn update(x: &mut String) { await sleep(1); x = "new" + " value"; gc_collect(); }
        fn mutate(x: Box) { x.value = "aliased"; }
        async fn main() {
            let b = new Box("old"); let alias = b;
            mutate(b); println(alias.value);
            await update(&b.value); println(alias.value);
            let values: Array<String> = ["old", "untouched"];
            await update(&values[0]); println(values[0]); println(values[1]);
        }"#;
    let (out, ok) = compile_and_run_gc_stress_mode(source, "relocate");
    assert!(ok, "{out}");
    assert_eq!(out, "aliased\nnew value\nnew value\nuntouched\n");
}

#[test]
fn async_gc_reference_diagnostics() {
    for (body, code) in [
        ("let x = \"old\"; await update(&x);", "E1701"),
        ("let mut x = \"old\"; await update(x);", "E1702"),
        ("await update(&(\"old\"));", "E1704"),
        ("let mut x = \"old\"; let t = update(&x);", "E1708"),
        (
            "let mut x = \"old\"; let t = update(&x); let u = update(&x); await t; await u;",
            "E1706",
        ),
    ] {
        let out = compile_error_stderr(&format!(
            "async fn update(x: &mut String) {{ await sleep(1); }} async fn main() {{ {body} }}"
        ));
        assert!(out.contains(code), "{body}: {out}");
        assert!(!out.contains("E1707"), "{out}");
    }
    for mode in ["&", "&mut"] {
        let source = format!(
            "import std::collections::Array; async fn use_array(x: {mode} Array<String>) {{ await sleep(1); }} async fn main() {{ let mut x: Array<String> = [\"old\"]; await use_array(&x); }}"
        );
        let out = compile_error_stderr(&source);
        assert!(out.contains("E2402") && out.contains("not `Sync`"), "{out}");
        assert!(!out.contains("E1707"), "{out}");
    }
    let out = compile_error_stderr(
        "class Box { pub value: String; } async fn update(x: &mut Box) { x.value = \"changed\"; } async fn main() { let mut x = new Box(\"old\"); await update(&x); }",
    );
    assert!(out.contains("E2402"), "{out}");
}

#[test]
fn async_gc_reference_methods_and_imports() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "model.wi",
                "module model; pub class Box { pub value: String; pub async fn update(self, x: &mut String) { await sleep(1); x = self.value; gc_collect(); } } pub async fn replace(x: &mut Box) { await sleep(1); x = new Box(\"replaced\"); gc_collect(); }",
            ),
            (
                "main.wi",
                "import model::Box; import model::replace; async fn local(x: &mut Box) { await replace(&x); } async fn main() { let mut b = new Box(\"method\"); let mut text = \"old\"; await b.update(&text); println(text); await local(&b); println(b.value); }",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "method\nreplaced\n");
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "model.wi",
                "module model; pub class Box { pub value: String; }",
            ),
            (
                "main.wi",
                "import model; async fn replace(x: &mut model::Box) { await sleep(1); x = new model::Box(\"qualified\"); gc_collect(); } async fn main() { let mut b = new model::Box(\"old\"); await replace(&b); println(b.value); }",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "qualified\n");
}

#[test]
fn async_gc_reference_shared_aliases() {
    let source = r#"
        class Box { pub value: String; }
        async fn replace(x: &mut Box, alias: Box) {
            await sleep(1); x = new Box("new"); gc_collect(); println(alias.value);
        }
        async fn read(x: &String) -> String { await sleep(1); gc_collect(); return x; }
        async fn main() {
            let mut b = new Box("old"); let alias = b;
            await replace(&b, alias); println(b.value); println(alias.value);
            let text = "shared" + " string";
            let a = read(&text); let c = read(&text);
            let first = await a; let second = await c; println(first); println(second);
        }
    "#;
    let (out, ok) = compile_and_run_gc_stress_mode(source, "relocate");
    assert!(ok, "{out}");
    assert_eq!(out, "old\nnew\nold\nshared string\nshared string\n");
}

#[test]
fn async_gc_reference_forwarding_scaling() {
    for depth in [1, 8, 32] {
        let mut source = String::from(
            "async fn f0(x: &mut String) { await sleep(1); x = \"new\" + \" value\"; gc_collect(); }",
        );
        for i in 1..depth {
            source.push_str(&format!(
                "async fn f{i}(x: &mut String) {{ await sleep(1); await f{}(&x); gc_collect(); }}",
                i - 1
            ));
        }
        source.push_str(&format!(
            "async fn main() {{ let mut x = \"old\"; await f{}(&x); println(x); }}",
            depth - 1
        ));
        let (out, ok) = compile_and_run_gc_stress_mode(&source, "relocate");
        assert!(ok, "depth={depth}: {out}");
        assert_eq!(out, "new value\n");
    }
}

#[test]
fn async_gc_reference_panic_and_cancel() {
    for cancel in [false, true] {
        let trigger = if cancel {
            "ready.send(1); await sleep(10000);"
        } else {
            "panic(\"stop\");"
        };
        let join = if cancel {
            "ready.recv(); s.cancel(); await s.finish();"
        } else {
            "await s.finish();"
        };
        let source = format!(
            "async fn work(x: &mut String, ready: Channel<i64>) {{
            defer match recover() {{ Some(_) => {{}}, None => {{}} }};
            defer {{ x = \"cleanup\" + \" value\"; gc_collect(); println(x); }}
            await sleep(1); {trigger}
        }} async fn main() {{ let mut x = \"old\"; let ready = Channel<i64>::new();
            let s = TaskScope::new(); s.add(work(&x, ready)); {join} println(x); }}"
        );
        let (out, ok) = compile_and_run_gc_stress_mode(&source, "relocate");
        assert!(ok, "cancel={cancel}: {out}");
        assert_eq!(out, "cleanup value\ncleanup value\n");
    }
}
