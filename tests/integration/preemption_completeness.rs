use crate::support::*;

#[test]
fn a7_example_runs() {
    let (out, ok) = compile_and_run(include_str!("../../example/preemption_complete.wi"));
    assert!(ok, "{out}");
    assert_eq!(out, "[41, 42]\n");
}

#[test]
fn a7_instantiated_bodies_keep_their_checked_proof() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "lib.wi",
                "pub fn id<T>(x:T)->T{return x;} pub interface Value<T> { fn raw(self)->T; fn get(self)->T{return self.raw();} }",
            ),
            (
                "main.wi",
                "import lib; class Box implements lib::Value<i64> { pub fn raw(self)->i64{return 42;} } async fn main(){println(lib::id(42));println(new Box().get());}",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n42\n");
}

// A7: these are executable acceptance cases, not just absence of E0810.
#[test]
fn a7_local_resolved_call_shapes() {
    for (name, declarations, body, expected) in [
        (
            "named",
            "fn f(x:i64)->i64 { return x+1; }",
            "println(f(41));",
            "42\n",
        ),
        (
            "function_argument",
            "fn f(x:i64)->i64 { return x+1; } fn apply(g:fn(i64)->i64)->i64 { return g(41); }",
            "println(apply(f));",
            "42\n",
        ),
        (
            "lambda_expression",
            "fn apply(g:fn(i64)->i64)->i64 { return g(41); }",
            "println(apply(|x| x+1));",
            "42\n",
        ),
        (
            "lambda_block",
            "fn apply(g:fn(i64)->i64)->i64 { return g(41); }",
            "println(apply(|x| { return x+1; }));",
            "42\n",
        ),
        (
            "array_constructor",
            "import std::collections::Array;",
            "let a:Array<i64> = Array::new(); a.push(42); println(a[0]);",
            "42\n",
        ),
        (
            "map_constructor",
            "import std::collections::Map;",
            "let m = Map::new(); m.insert(1,42); println(m.get(1).unwrap());",
            "42\n",
        ),
        (
            "implicit_constructor",
            "class Box { pub x:i64; pub fn get(self)->i64 { return self.x; } }",
            "println(new Box(42).get());",
            "42\n",
        ),
        (
            "interface",
            "interface I { fn get(self)->i64; } class Box implements I { pub fn get(self)->i64 { return 42; } } fn read(i:I)->i64 { return i.get(); }",
            "println(read(new Box()));",
            "42\n",
        ),
        (
            "default_method",
            "interface I { fn get(self)->i64 { return 42; } } class Box implements I {}",
            "println(new Box().get());",
            "42\n",
        ),
        (
            "open_self",
            "open class Base { pub open fn value(self)->i64 { return 1; } pub fn run(self)->i64 { return self.value(); } } class Derived extends Base { pub override fn value(self)->i64 { return 42; } }",
            "println(new Derived().run());",
            "42\n",
        ),
        (
            "recursive",
            "fn f(n:i64)->i64 { if n==0 { return 42; } return f(n-1); }",
            "println(f(5));",
            "42\n",
        ),
        (
            "sync_loop",
            "fn f()->i64 { let mut n=0; while n<42 { n=n+1; } return n; }",
            "println(f());",
            "42\n",
        ),
    ] {
        let source = format!("{declarations}\nasync fn main() {{ {body} }}");
        let (out, ok) = compile_and_run(&source);
        assert!(ok, "{name}: {out}");
        assert_eq!(out, expected, "{name}");
    }
}

const LIBRARY: &str = r#"
pub interface Value { fn get(self)->i64; }
pub fn read(v:Value)->i64 { return v.get(); }
pub fn apply(f:fn(i64)->i64)->i64 { return f(41); }
pub fn add(x:i64)->i64 { return x+1; }
pub class Box { pub x:i64; pub fn get(self)->i64 { return self.x; } }
pub open class Base { pub open fn get(self)->i64 { return 1; } pub fn run(self)->i64 { return self.get(); } }
pub fn run(b:Base)->i64 { return b.get(); }
pub interface Default { fn get(self)->i64 { return 42; } }
pub class DefaultBox implements Default {}
"#;

#[test]
fn a7_cross_module_resolved_call_shapes() {
    for (name, source) in [
        (
            "module_helper",
            "import lib; async fn main(){ println(lib::add(41)); }",
        ),
        (
            "item_alias",
            "import lib::add as f; async fn main(){ println(f(41)); }",
        ),
        (
            "concrete_method",
            "import lib; async fn main(){ println(new lib::Box(42).get()); }",
        ),
        (
            "class_alias",
            "import lib::Box as B; async fn main(){ println(new B(42).get()); }",
        ),
        (
            "callback_from_entry",
            "import lib; fn plus(x:i64)->i64{return x+1;} async fn main(){println(lib::apply(plus));}",
        ),
        (
            "callback_from_module",
            "import lib::add; fn apply(f:fn(i64)->i64)->i64{return f(41);} async fn main(){println(apply(add));}",
        ),
        (
            "interface_from_entry",
            "import lib; class Box implements lib::Value { pub fn get(self)->i64{return 42;} } async fn main(){println(lib::read(new Box()));}",
        ),
        (
            "override_from_entry",
            "import lib; class Derived extends lib::Base { pub override fn get(self)->i64{return 42;} } async fn main(){println(lib::run(new Derived()));}",
        ),
        (
            "self_override_from_entry",
            "import lib; class Derived extends lib::Base { pub override fn get(self)->i64{return 42;} } async fn main(){println(new Derived().run());}",
        ),
        (
            "imported_default",
            "import lib; async fn main(){println(new lib::DefaultBox().get());}",
        ),
    ] {
        let (out, ok) =
            compile_temp_project_and_run(&[("lib.wi", LIBRARY), ("main.wi", source)], "main.wi");
        assert!(ok, "{name}: {out}");
        assert_eq!(out, "42\n", "{name}");
    }
}

#[test]
fn a7_unresolved_obligations_are_diagnosed() {
    for (name, source) in [
        (
            "empty_interface",
            "interface I extends Sync { fn get(self)->i64; } async fn run(i:I)->i64{return i.get();} fn main(){}",
        ),
        (
            "empty_callback",
            "async fn run(f:fn(i64)->i64)->i64{return f(41);} fn main(){}",
        ),
        (
            "transitive_callback",
            "fn helper(f:fn(i64)->i64)->i64{return f(41);} async fn run(f:fn(i64)->i64)->i64{return helper(f);} fn main(){}",
        ),
    ] {
        let (ok, errors) = compile_with_compiler_env(source, &[]);
        assert!(!ok, "{name}");
        assert!(errors.contains("error[E0810]"), "{name}: {errors}");
        assert!(
            errors.contains("unresolved call target"),
            "{name}: {errors}"
        );
    }
}

#[test]
fn a7_imported_unknown_summary_is_not_silently_dropped() {
    let errors = compile_temp_project_error_stderr(
        &[
            (
                "lib.wi",
                "pub interface I extends Sync { fn get(self)->i64; } pub fn read(i:I)->i64{return i.get();}",
            ),
            (
                "main.wi",
                "import lib; async fn run(i:lib::I)->i64{return lib::read(i);} fn main(){}",
            ),
        ],
        "main.wi",
    );
    assert!(errors.contains("error[E0810]"), "{errors}");
    assert!(errors.contains("unresolved call target"), "{errors}");
}

#[test]
fn a7_adt_fallback_callbacks_remain_proven() {
    for (name, body, expected) in [
        (
            "option_absent",
            "let x:Option<i64>=None; println(x.or_else(|| { let y:Option<i64>=Some(42); return y; }).unwrap());",
            "42\n",
        ),
        (
            "option_present",
            "let x:Option<i64>=Some(7); println(x.or_else(|| { let y:Option<i64>=Some(42); return y; }).unwrap());",
            "7\n",
        ),
        (
            "result_error",
            "let x:Result<i64,bool>=Err(false); println(x.or_else(|e| { let y:Result<i64,bool>=Ok(42); return y; }).unwrap());",
            "42\n",
        ),
        (
            "result_ok",
            "let x:Result<i64,bool>=Ok(7); println(x.or_else(|e| { let y:Result<i64,bool>=Ok(42); return y; }).unwrap());",
            "7\n",
        ),
        (
            "unwrap_error",
            "let x:Result<i64,bool>=Err(true); println(x.unwrap_err());",
            "true\n",
        ),
    ] {
        let (out, ok) = compile_and_run(&format!("async fn main() {{ {body} }}"));
        assert!(ok, "{name}: {out}");
        assert_eq!(out, expected, "{name}");
    }
}

#[test]
fn a7_adt_fallback_does_not_hide_an_unknown_callback() {
    for source in [
        "async fn run(f:fn()->Option<i64>)->Option<i64>{let x:Option<i64>=None;return x.or_else(f);} fn main(){}",
        "async fn run(f:fn(bool)->Result<i64,bool>)->Result<i64,bool>{let x:Result<i64,bool>=Err(false);return x.or_else(f);} fn main(){}",
    ] {
        let (ok, errors) = compile_with_compiler_env(source, &[]);
        assert!(!ok);
        assert!(errors.contains("error[E0810]"), "{errors}");
    }
}

#[test]
fn jz15_80_imported_generic_task_repro() {
    let project = TestProject::new(
        "jz15_80",
        &[
            ("boxes.wi", "pub fn id<T>(x:T)->T{return x;}"),
            (
                "main.wi",
                "import boxes::id; async fn main(){println(id(4));}",
            ),
        ],
    );
    let check = project.check("main.wi");
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let build = project.compile("main.wi");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let run = project.run_with_env_timeout(&[], std::time::Duration::from_secs(10));
    assert!(run.status.success());
    assert_eq!(String::from_utf8_lossy(&run.stdout), "4\n");
}

// Twenty explicit combinations: item/module imports and aliases, each with
// inferred scalar/String/bool, explicit type arguments, and repeated instances.
#[test]
fn jz15_80_imported_generic_task_matrix() {
    for (import, callee) in [
        ("import boxes::id;", "id"),
        ("import boxes::id as identity;", "identity"),
        ("import boxes;", "boxes::id"),
        ("import boxes as b;", "b::id"),
    ] {
        for (body, expected) in [
            (format!("println({callee}(4));"), "4\n"),
            (format!("println({callee}(\"kept\"));"), "kept\n"),
            (format!("println({callee}(true));"), "true\n"),
            (format!("println({callee}<i64>(7));"), "7\n"),
            (
                format!("println({callee}(4));println({callee}(5));println({callee}(false));"),
                "4\n5\nfalse\n",
            ),
        ] {
            let source = format!("{import} async fn main(){{{body}}}");
            let project = TestProject::new(
                "jz15_80_matrix",
                &[
                    ("boxes.wi", "pub fn id<T>(x:T)->T{return x;}"),
                    ("main.wi", &source),
                ],
            );
            let check = project.check("main.wi");
            assert!(
                check.status.success(),
                "{source}: {}",
                String::from_utf8_lossy(&check.stderr)
            );
            let build = project.compile("main.wi");
            assert!(
                build.status.success(),
                "{source}: {}",
                String::from_utf8_lossy(&build.stderr)
            );
            let run = project.run_with_env_timeout(&[], std::time::Duration::from_secs(10));
            assert!(run.status.success(), "{source}");
            assert_eq!(String::from_utf8_lossy(&run.stdout), expected, "{source}");
        }
    }
}

#[test]
fn jz15_80_generic_unknown_has_source_name_and_call_span() {
    let project = TestProject::new(
        "jz15_80_diagnostic",
        &[
            (
                "boxes.wi",
                "pub fn apply<T>(x:T,f:fn(T)->T)->T{return f(x);}",
            ),
            (
                "main.wi",
                "import boxes::apply;\nasync fn run(f:fn(i64)->i64)->i64 {\n    return apply(4,f);\n}\nfn main(){}",
            ),
        ],
    );
    for output in [project.check("main.wi"), project.compile("main.wi")] {
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{error}");
        assert!(error.contains("error[E0810]"), "{error}");
        assert!(error.contains("main.wi:3:"), "{error}");
        assert!(!error.contains("$mono$"), "{error}");
    }
}

#[test]
fn jz15_80_imported_generic_example_runs() {
    let (out, ok) = compile_file_and_run("example/imported_generic_preemption.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "4\nkept\n");
}

#[test]
fn jz15_80_proven_indirect_call_does_not_hide_failing_generic() {
    let project = TestProject::new(
        "jz15_80_safe_indirect",
        &[
            (
                "boxes.wi",
                "pub fn apply<T>(x:T,f:fn(T)->T)->T{return f(x);}",
            ),
            (
                "main.wi",
                "import boxes::apply;\nfn safe()->i64{return 1;}\nasync fn run(f:fn(i64)->i64)->i64 {\n    let g = safe;\n    println(g());\n    return apply(4,f);\n}\nfn main(){}",
            ),
        ],
    );
    for output in [project.check("main.wi"), project.compile("main.wi")] {
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{error}");
        assert!(error.contains("error[E0810]"), "{error}");
        assert!(error.contains("main.wi:6:"), "{error}");
        assert!(!error.contains("main.wi:5:"), "{error}");
        assert!(!error.contains("$mono$"), "{error}");
    }
}

#[test]
fn jz15_80_virtual_spans_follow_build_wide_dispatch() {
    for api in [
        "pub interface I extends Sync { fn get(self)->i64; }",
        "pub interface I extends Sync { fn get(self)->i64{return 1;} }",
    ] {
        for (source, expected_line, safe_line) in [
            (
                "import api; import impls::touch; import boxes::apply;\nasync fn run(i:api::I,f:fn(i64)->i64)->i64 {\n    println(i.get());\n    return apply(4,f);\n}\nfn main(){touch();}",
                4,
                Some(3),
            ),
            (
                "import api;\nasync fn run(i:api::I)->i64 {\n    return i.get();\n}\nfn main(){}",
                3,
                None,
            ),
        ] {
            let project = TestProject::new(
                "jz15_80_virtual",
                &[
                    ("api.wi", api),
                    (
                        "impls.wi",
                        "import api; pub class Box implements api::I { pub fn get(self)->i64{return 1;} } pub fn touch(){let b=new Box();}",
                    ),
                    (
                        "boxes.wi",
                        "pub fn apply<T>(x:T,f:fn(T)->T)->T{return f(x);}",
                    ),
                    ("main.wi", source),
                ],
            );
            for output in [project.check("main.wi"), project.compile("main.wi")] {
                let error = String::from_utf8_lossy(&output.stderr);
                assert!(!output.status.success(), "{error}");
                assert!(error.contains("error[E0810]"), "{error}");
                assert!(
                    error.contains(&format!("main.wi:{expected_line}:")),
                    "{error}"
                );
                if let Some(line) = safe_line {
                    assert!(!error.contains(&format!("main.wi:{line}:")), "{error}");
                }
            }
        }
    }
}

#[test]
fn jz15_80_virtual_override_hazard_uses_union_span() {
    let project = TestProject::new(
        "jz15_80_override",
        &[
            (
                "api.wi",
                "pub open class Base { pub open fn get(self)->i64{return 1;} }",
            ),
            (
                "impls.wi",
                "import api; extern rust {fn foreign()->i64;} pub class Box extends api::Base { pub override fn get(self)->i64{return foreign();} } pub fn touch(){let b=new Box();}",
            ),
            (
                "main.wi",
                "import api; import impls::touch;\nasync fn run(b:api::Base)->i64 {\n    return b.get();\n}\nfn main(){touch();}",
            ),
        ],
    );
    for output in [project.check("main.wi"), project.compile("main.wi")] {
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{error}");
        assert!(error.contains("error[E0810]"), "{error}");
        assert!(error.contains("main.wi:3:"), "{error}");
    }
}
