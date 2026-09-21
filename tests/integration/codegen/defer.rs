use super::*;

// ── defer (willow-vynv.2, sync Phase 2) ─────────────────────────────────────
// Scope-based (Zig-style, per-iteration in loops), LIFO, operands evaluated
// at registration. Runs on: fallthrough, return, `?`, break, continue. Not
// on panic (abort, no unwind). 20 perspectives: 1 runs at scope end, 2 LIFO
// order, 3 runs before return's caller observes (value computed first),
// 4 args evaluated at registration, 5 receiver evaluated at registration,
// 6 method-call defer, 7 print defer, 8 `?` propagation runs defers,
// 9 Ok-path `?` does NOT early-run them, 10 break flushes loop-scope
// defers, 11 continue flushes per-iteration defers, 12 loop body defer runs
// EACH iteration, 13 nested scopes flush inner-first, 14 inner-scope defer
// runs at inner exit not fn end, 15 non-call rejected E0905, 16 async fn
// defer rejected E0905, 17 GC stress with String operands (rooted slots),
// 18 defer in main with Result main, 19 class method named `close` resolves
// to the class (not the channel builtin) — found via defer, 20 defer does
// NOT run on panic.

#[test]
fn dfr_01_scope_end() {
    let (out, ok) = compile_and_run("fn c() { println(2); }\nfn main() { defer c(); println(1); }");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn dfr_02_lifo() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nfn main() { defer c(1); defer c(2); defer c(3); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n2\n1\n");
}

#[test]
fn dfr_03_return_value_computed_first() {
    let (out, ok) = compile_and_run(
        "fn c() { println(8); }\nfn f() -> i64 { defer c(); return 5; }\nfn main() { println(f()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n5\n");
}

#[test]
fn dfr_04_args_evaluated_at_registration() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nfn main() { let mut x = 1; defer c(x); x = 99; println(x); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n1\n");
}

#[test]
fn dfr_05_receiver_evaluated_at_registration() {
    let (out, ok) = compile_and_run(
        "class R { pub v: i64; pub fn show(self) { println(self.v); } }\nfn main() { let mut r = new R(1); defer r.show(); r = new R(2); println(r.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n1\n");
}

#[test]
fn dfr_06_method_call() {
    let (out, ok) = compile_and_run(
        "class Res { pub name: String; pub fn close(self) { println(\"closed \" + self.name); } }\nfn main() { let r = new Res(\"db\"); defer r.close(); println(\"work\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "work\nclosed db\n");
}

#[test]
fn dfr_07_print_defer() {
    let (out, ok) = compile_and_run("fn main() { let x = 5; defer println(x); println(1); }");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n5\n");
}

#[test]
fn dfr_08_try_propagation_runs_defers() {
    let (out, ok) = compile_and_run(
        "fn c() { println(77); }\nfn bad() -> Result<i64, String> { return Err(\"e\"); }\nfn f() -> Result<i64, String> { defer c(); let x = bad()?; return Ok(x); }\nfn main() { match f() { Ok(v) => println(v), Err(e) => println(e), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "77\ne\n");
}

#[test]
fn dfr_09_ok_path_no_early_run() {
    let (out, ok) = compile_and_run(
        "fn c() { println(77); }\nfn good() -> Result<i64, String> { return Ok(3); }\nfn f() -> Result<i64, String> { defer c(); let x = good()?; println(x); return Ok(x); }\nfn main() { match f() { Ok(v) => println(v), Err(e) => println(e), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n77\n3\n");
}

#[test]
fn dfr_10_break_flushes() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nfn main() { for i in 0..5 { defer c(i); if i == 1 { break; } } println(9); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n9\n");
}

#[test]
fn dfr_11_continue_flushes() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n * 10); }\nfn main() { for i in 0..3 { defer c(i); if i == 1 { continue; } println(i); } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n0\n10\n2\n20\n");
}

#[test]
fn dfr_12_loop_body_per_iteration() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nfn main() { for i in 0..3 { defer c(i); } println(9); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n2\n9\n");
}

#[test]
fn dfr_13_nested_scopes_inner_first_on_return() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nfn f() { defer c(1); if true { defer c(2); return; } }\nfn main() { f(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n1\n");
}

#[test]
fn dfr_14_inner_scope_exits_early() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nfn main() { if true { defer c(1); println(0); } println(2); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n2\n");
}

#[test]
fn dfr_15_non_call_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { defer 1 + 2; }", &[]);
    assert!(!ok);
    assert!(stderr.contains("E0905"), "{stderr}");
}

#[test]
fn dfr_16_async_defer_now_allowed() {
    // Phase 3 (willow-vynv.3) lifted the async restriction: defer in an
    // async fn registers into the frame and flushes on exit.
    let (out, ok) = compile_and_run("fn c() { println(2); }\nfn main() { defer c(); println(1); }");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn dfr_17_gc_stress_string_operands() {
    let (out, ok) = compile_and_run_gc_stress(
        "fn c(s: String) { println(s); }\nfn main() { let name = \"a\" + \"b\"; defer c(name + \"!\"); println(\"work\" + \"s\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "works\nab!\n");
}

#[test]
fn dfr_18_result_main() {
    let (out, ok) = compile_and_run(
        "fn c() { println(1); }\nfn main() -> Result<void, String> { defer c(); println(0); return Result::Ok(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n");
}

#[test]
fn dfr_19_class_close_not_channel_builtin() {
    let (out, ok) = compile_and_run(
        "class C { pub fn close(self) { println(4); } }\nfn main() { let c = new C(); defer c.close(); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n4\n");
}

#[test]
fn dfr_20_runs_before_unhandled_panic_finishes() {
    let (out, ok) = compile_and_run_check_exit(
        "fn c() { println(111); }\nfn main() { defer c(); panic(\"stop\"); }",
    );
    assert!(!ok);
    assert!(
        out.contains("111"),
        "defer must run during panic unwind: {out}"
    );
    assert!(out.contains("runtime panic: stop"), "{out}");
}

// Review fixes on sync defer (willow-vynv.2): two silent-misbehavior holes
// closed as compile errors. 21 reference args would mutate the hidden
// registration-time COPY, not the caller's variable; 22 an async callee
// would only SPAWN a task at scope exit (cleanup body never driven);
// 23 same for Future-returning runtime calls (sleep).

#[test]
fn dfr_21_reference_arg_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn inc(n: &mut i64) { n = n + 1; }\nfn main() { let mut n = 1; if true { defer inc(&n); } println(n); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E0905"), "{stderr}");
    assert!(stderr.contains("reference arguments"), "{stderr}");
}

#[test]
fn dfr_22_async_callee_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn cleanup() { println(9); }\nfn main() { defer cleanup(); println(1); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("async call"), "{stderr}");
}

#[test]
fn dfr_23_future_callee_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { defer sleep(5); }", &[]);
    assert!(!ok);
    assert!(stderr.contains("async call"), "{stderr}");
}

#[test]
fn dfr_24_missing_semicolon_rejected() {
    assert_dfr_compile_error("fn c() {}\nfn main() { defer c() }", "E0101");
}

#[test]
fn dfr_25_empty_defer_rejected() {
    assert_dfr_compile_fails("fn main() { defer; }");
}

#[test]
fn dfr_26_variable_defer_rejected() {
    assert_dfr_compile_error("fn main() { let x = 1; defer x; }", "E0905");
}

#[test]
fn dfr_27_match_expr_defer_allowed() {
    assert_dfr_runs(
        "fn main() { defer match 1 { _ => println(2), }; println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_28_unknown_function_rejected() {
    assert_dfr_compile_error("fn main() { defer missing_cleanup(); }", "E0350");
}

#[test]
fn dfr_29_wrong_arg_count_rejected() {
    assert_dfr_compile_error("fn c(n: i64) {}\nfn main() { defer c(); }", "E0201");
}

#[test]
fn dfr_30_wrong_arg_type_rejected() {
    assert_dfr_compile_error("fn c(n: i64) {}\nfn main() { defer c(true); }", "E0201");
}

#[test]
fn dfr_31_method_not_found_rejected() {
    assert_dfr_compile_error(
        "class C {}\nfn main() { let c = new C(); defer c.close(); }",
        "E0502",
    );
}

#[test]
fn dfr_32_print_without_newline() {
    assert_dfr_runs("fn main() { defer print(5); println(1); }", "1\n5");
}

#[test]
fn dfr_33_sync_lambda_body() {
    assert_dfr_runs(
        "fn main() { let f = || { defer println(2); println(1); }; f(); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_34_constructor_body() {
    assert_dfr_runs(
        "class R { pub init(self) { defer println(2); println(1); } }\nfn main() { let r = new R(); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_35_static_method_body() {
    assert_dfr_runs(
        "class R { pub static fn run() { defer println(2); println(1); } }\nfn main() { R::run(); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_36_empty_block_no_effect() {
    assert_dfr_runs("fn main() { if true {} println(1); }", "1\n");
}

#[test]
fn dfr_37_if_then_only() {
    assert_dfr_runs(
        "fn main() { if true { defer println(1); } else { defer println(2); } println(3); }",
        "1\n3\n",
    );
}

#[test]
fn dfr_38_if_else_only() {
    assert_dfr_runs(
        "fn main() { if false { defer println(1); } else { defer println(2); } println(3); }",
        "2\n3\n",
    );
}

#[test]
fn dfr_39_branch_selection_does_not_register_other_side() {
    assert_dfr_runs(
        "fn main() { let flag = false; if flag { defer println(1); } else { println(2); } println(3); }",
        "2\n3\n",
    );
}

#[test]
fn dfr_40_branch_defer_before_outer_defer() {
    assert_dfr_runs(
        "fn main() { defer println(9); if true { defer println(1); } println(2); }",
        "1\n2\n9\n",
    );
}

#[test]
fn dfr_41_nested_if_lifo() {
    assert_dfr_runs(
        "fn main() { defer println(9); if true { defer println(1); if true { defer println(2); } } }",
        "2\n1\n9\n",
    );
}

#[test]
fn dfr_42_bare_return_flushes() {
    assert_dfr_runs(
        "fn f() { defer println(1); return; println(9); }\nfn main() { f(); println(2); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_43_void_return_flushes() {
    assert_dfr_runs(
        "fn f() { defer println(1); if true { return; } println(9); }\nfn main() { f(); println(2); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_44_gc_return_value_survives_defer() {
    assert_dfr_runs_gc_stress(
        "fn cleanup() { println(\"cleanup\"); }\nfn f() -> String { let s = \"a\" + \"b\"; defer cleanup(); return s + \"!\"; }\nfn main() { println(f()); }",
        "cleanup\nab!\n",
    );
}

#[test]
fn dfr_45_result_return_err_flushes() {
    assert_dfr_runs(
        "fn cleanup() { println(1); }\nfn f() -> Result<i64, String> { defer cleanup(); return Result::Err(\"bad\"); }\nfn main() { match f() { Result::Ok(v) => println(v), Result::Err(e) => println(e), } }",
        "1\nbad\n",
    );
}

#[test]
fn dfr_46_result_main_err_flushes() {
    assert_dfr_exit_contains(
        "fn main() -> Result<void, String> { defer println(1); return Result::Err(\"bad\"); }",
        &["1\n", "bad"],
    );
}

#[test]
fn dfr_47_option_none_try_flushes() {
    assert_dfr_runs(
        "fn cleanup() { println(1); }\nfn none() -> Option<i64> { return Option::None; }\nfn f() -> Option<i64> { defer cleanup(); let v = none()?; return Option::Some(v); }\nfn main() { match f() { Option::Some(v) => println(v), Option::None => println(0), } }",
        "1\n0\n",
    );
}

#[test]
fn dfr_48_option_some_try_no_early_flush() {
    assert_dfr_runs(
        "fn cleanup() { println(1); }\nfn some() -> Option<i64> { return Option::Some(3); }\nfn f() -> Option<i64> { defer cleanup(); let v = some()?; println(v); return Option::Some(v); }\nfn main() { match f() { Option::Some(v) => println(v), Option::None => println(0), } }",
        "3\n1\n3\n",
    );
}

#[test]
fn dfr_49_callee_try_does_not_flush_caller() {
    assert_dfr_runs(
        "fn cleanup(n: i64) { println(n); }\nfn bad() -> Result<i64, String> { return Result::Err(\"e\"); }\nfn inner() -> Result<i64, String> { defer cleanup(1); let v = bad()?; return Result::Ok(v); }\nfn outer() { defer cleanup(9); match inner() { Result::Ok(v) => println(v), Result::Err(e) => println(e), } println(0); }\nfn main() { outer(); }",
        "1\ne\n0\n9\n",
    );
}

#[test]
fn dfr_50_match_arm_return_flushes() {
    assert_dfr_runs(
        "fn f(n: i64) { defer println(9); match n { 0 => { defer println(1); return; }, _ => println(3), } }\nfn main() { f(0); }",
        "1\n9\n",
    );
}

#[test]
fn dfr_51_match_arm_try_flushes() {
    assert_dfr_runs(
        "fn bad() -> Result<i64, String> { return Result::Err(\"e\"); }\nfn f(n: i64) -> Result<i64, String> { defer println(9); match n { 0 => { defer println(1); let x = bad()?; return Result::Ok(x); } _ => { } } return Result::Ok(2); }\nfn main() { match f(0) { Result::Ok(v) => println(v), Result::Err(e) => println(e), } }",
        "1\n9\ne\n",
    );
}

#[test]
fn dfr_52_match_normal_path_waits_for_scope_exit() {
    assert_dfr_runs(
        "fn main() { match 1 { 1 => { defer println(1); println(7); } _ => { } } println(2); }",
        "7\n1\n2\n",
    );
}

#[test]
fn dfr_53_try_error_conversion_runs_defer() {
    assert_dfr_runs(
        "class HighErr { pub code: i64; }\nclass LowErr implements Into<HighErr> { pub n: i64; pub fn into(self) -> HighErr { return new HighErr(self.n + 10); } }\nfn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(5)); }\nfn high() -> Result<i64, HighErr> { defer println(1); let v = low()?; return Result::Ok(v); }\nfn main() { match high() { Result::Ok(v) => println(v), Result::Err(e) => println(e.code), } }",
        "1\n15\n",
    );
}

#[test]
fn dfr_54_deferred_function_panic_reports_after_prior_output() {
    assert_dfr_exit_contains(
        "fn cleanup() { panic(\"boom\"); }\nfn main() { defer cleanup(); println(1); }",
        &["1\n", "boom"],
    );
}

#[test]
fn dfr_55_registration_panic_prevents_later_statements() {
    assert_dfr_exit_contains(
        "fn arg() -> i64 { panic(\"arg\"); return 0; }\nfn cleanup(n: i64) { println(n); }\nfn main() { defer cleanup(arg()); println(1); }",
        &["arg"],
    );
}

#[test]
fn dfr_56_try_in_direct_defer_argument_rejected() {
    assert_dfr_compile_error(
        "fn cleanup(n: i64) { println(n); }\nfn bad() -> Result<i64, String> { return Result::Err(\"e\"); }\nfn f() -> Result<void, String> { defer cleanup(1); defer cleanup(bad()?); return Result::Ok(); }\nfn main() { match f() { Result::Ok(_) => println(0), Result::Err(e) => println(e), } }",
        "E0905",
    );
}

#[test]
fn dfr_57_for_wildcard_flushes_each_iteration() {
    assert_dfr_runs(
        "fn main() { for _ in 0..2 { defer println(7); } println(9); }",
        "7\n7\n9\n",
    );
}

#[test]
fn dfr_58_while_body_per_iteration() {
    assert_dfr_runs(
        "fn main() { let mut i = 0; while i < 2 { defer println(i); i = i + 1; } println(9); }",
        "0\n1\n9\n",
    );
}

#[test]
fn dfr_59_while_break_flushes() {
    assert_dfr_runs(
        "fn main() { let mut i = 0; while true { defer println(i); if i == 1 { break; } i = i + 1; } println(9); }",
        "0\n1\n9\n",
    );
}

#[test]
fn dfr_60_while_continue_flushes() {
    assert_dfr_runs(
        "fn main() { let mut i = 0; while i < 3 { defer println(i); i = i + 1; if i == 2 { continue; } println(8); } }",
        "8\n0\n1\n8\n2\n",
    );
}

#[test]
fn dfr_61_nested_loop_break_flushes_inner_only() {
    assert_dfr_runs(
        "fn c(n: i64) { println(n); }\nfn main() { defer c(9); for i in 0..2 { defer c(10 + i); for j in 0..2 { defer c(j); break; } println(5); } }",
        "0\n5\n10\n0\n5\n11\n9\n",
    );
}

#[test]
fn dfr_62_nested_loop_continue_flushes_inner_only() {
    assert_dfr_runs(
        "fn c(n: i64) { println(n); }\nfn main() { for i in 0..1 { defer c(9); for j in 0..2 { defer c(j); if j == 0 { continue; } println(7); } println(8); } }",
        "0\n7\n1\n8\n9\n",
    );
}

#[test]
fn dfr_63_inner_block_defer_runs_before_loop_defer() {
    assert_dfr_runs(
        "fn main() { for _ in 0..1 { defer println(1); if true { defer println(2); } } }",
        "2\n1\n",
    );
}

#[test]
fn dfr_64_range_for_captures_iteration_value() {
    assert_dfr_runs(
        "fn main() { for i in 0..3 { defer println(i); } println(9); }",
        "0\n1\n2\n9\n",
    );
}

#[test]
fn dfr_65_array_for_defer() {
    assert_dfr_runs(
        "fn main() { let a = [4, 5]; for x in a { defer println(x); } println(9); }",
        "4\n5\n9\n",
    );
}

#[test]
fn dfr_66_array_for_continue_defer() {
    assert_dfr_runs(
        "fn main() { let a = [1, 2]; for x in a { defer println(x); continue; } println(9); }",
        "1\n2\n9\n",
    );
}

#[test]
fn dfr_67_string_argument() {
    assert_dfr_runs(
        "fn c(s: String) { println(s); }\nfn main() { let s = \"x\"; defer c(s + \"y\"); println(\"z\"); }",
        "z\nxy\n",
    );
}

#[test]
fn dfr_68_array_argument_len() {
    assert_dfr_runs(
        "import std::collections::Array;\nfn c(a: Array<i64>) { println(a.len()); }\nfn main() { let a: Array<i64> = [1, 2, 3]; defer c(a); println(0); }",
        "0\n3\n",
    );
}

#[test]
fn dfr_69_class_object_argument() {
    assert_dfr_runs(
        "class C { pub v: i64; }\nfn show(c: C) { println(c.v); }\nfn main() { let c = new C(4); defer show(c); println(1); }",
        "1\n4\n",
    );
}

#[test]
fn dfr_70_enum_payload_argument() {
    assert_dfr_runs(
        "enum Kind { Small, Big(i64) }\nfn show(k: Kind) { let out = match k { Kind::Big(v) => v, Kind::Small => 0, }; println(out); }\nfn main() { defer show(Kind::Big(7)); println(1); }",
        "1\n7\n",
    );
}

#[test]
fn dfr_71_option_object_after_match() {
    assert_dfr_runs(
        "class C { pub fn show(self) { println(2); } }\nfn main() { let x: Option<C> = Some(new C()); match x { Some(value) => { defer value.show(); println(1); }, None => {} } }",
        "1\n2\n",
    );
}

#[test]
fn dfr_72_interface_dispatch() {
    assert_dfr_runs(
        "interface I { fn show(self); }\nclass C implements I { pub fn show(self) { println(2); } }\nfn main() { let x: I = new C(); defer x.show(); println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_73_inherited_method_dispatch() {
    assert_dfr_runs(
        "open class Base { pub fn show(self) { println(2); } }\nclass Child extends Base {}\nfn main() { let x = new Child(); defer x.show(); println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_74_overridden_method_dispatch() {
    assert_dfr_runs(
        "open class Base { pub open fn show(self) { println(2); } }\nclass Child extends Base { pub override fn show(self) { println(3); } }\nfn main() { let x = new Child(); defer x.show(); println(1); }",
        "1\n3\n",
    );
}

#[test]
fn dfr_75_static_call_rejected() {
    assert_dfr_compile_error(
        "class C { pub static fn cleanup() {} }\nfn main() { defer C::cleanup(); }",
        "E0905",
    );
}

#[test]
fn dfr_76_static_constructor_call_rejected() {
    assert_dfr_compile_error("fn main() { defer Channel<i64>::new(); }", "E0905");
}

#[test]
fn dfr_77_indirect_function_value() {
    assert_dfr_runs(
        "fn c() { println(2); }\nfn main() { let f = c; defer f(); println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_78_lambda_value() {
    assert_dfr_runs(
        "fn main() { let f = || { println(2); }; defer f(); println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_79_multiple_argument_types() {
    assert_dfr_runs(
        "fn c(n: i64, b: bool, f: f64, s: String) { println(n); println(b); println(f); println(s); }\nfn main() { defer c(1, true, 2.5, \"x\"); println(0); }",
        "0\n1\ntrue\n2.5\nx\n",
    );
}

#[test]
fn dfr_80_method_argument_captured() {
    assert_dfr_runs(
        "class C { pub fn show(self, n: i64) { println(n); } }\nfn main() { let c = new C(); let mut n = 1; defer c.show(n); n = 2; println(n); }",
        "2\n1\n",
    );
}

#[test]
fn dfr_81_receiver_object_field_mutation_visible() {
    assert_dfr_runs(
        "class C { pub v: i64; pub fn show(self) { println(self.v); } }\nfn main() { let c = new C(1); defer c.show(); c.v = 2; println(0); }",
        "0\n2\n",
    );
}

#[test]
fn dfr_82_argument_side_effect_at_registration() {
    assert_dfr_runs(
        "fn mark(n: i64) -> i64 { println(n); return n; }\nfn c(n: i64) { println(n * 10); }\nfn main() { defer c(mark(1)); println(2); }",
        "1\n2\n10\n",
    );
}

#[test]
fn dfr_83_call_side_effect_at_exit() {
    assert_dfr_runs(
        "fn c() { println(2); }\nfn main() { defer c(); println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_84_deferred_return_value_ignored() {
    assert_dfr_runs(
        "fn c() -> i64 { println(2); return 9; }\nfn main() { defer c(); println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_85_print_bool_and_float() {
    assert_dfr_runs(
        "fn main() { defer println(true); defer println(2.5); println(1); }",
        "1\n2.5\ntrue\n",
    );
}

#[test]
fn dfr_86_none_branch_does_not_register_defer() {
    assert_dfr_runs(
        "class C { pub fn show(self) { println(2); } }\nfn main() { let x: Option<C> = None; match x { Some(value) => { defer value.show(); }, None => {} } println(1); }",
        "1\n",
    );
}

#[test]
fn dfr_87_to_string_argument() {
    assert_dfr_runs(
        "fn c(s: String) { println(s); }\nfn main() { let x = 5; defer c(x.toString()); println(1); }",
        "1\n5\n",
    );
}

#[test]
fn dfr_88_inner_scope_gc_stress_after_flush() {
    assert_dfr_runs_gc_stress(
        "fn c(s: String) { println(s); }\nfn main() { if true { let s = \"a\" + \"b\"; defer c(s); } println(\"c\" + \"d\"); }",
        "ab\ncd\n",
    );
}

#[test]
fn dfr_89_many_defers_lifo() {
    assert_dfr_runs(
        "fn main() { defer println(1); defer println(2); defer println(3); defer println(4); defer println(5); println(0); }",
        "0\n5\n4\n3\n2\n1\n",
    );
}

#[test]
fn dfr_90_hidden_name_collision_does_not_shadow_user_var() {
    assert_dfr_runs(
        "fn c(n: i64) { println(n); }\nfn main() { let __defer0_a0 = 7; defer c(1); println(__defer0_a0); }",
        "7\n1\n",
    );
}

#[test]
fn dfr_91_nested_function_scope_separation() {
    assert_dfr_runs(
        "fn c(n: i64) { println(n); }\nfn inner() { defer c(1); }\nfn main() { defer c(9); inner(); println(0); }",
        "1\n0\n9\n",
    );
}

#[test]
fn dfr_92_defer_in_if_then_return_flushes_inner_first() {
    assert_dfr_runs(
        "fn c(n: i64) { println(n); }\nfn f() { defer c(9); if true { defer c(1); return; } }\nfn main() { f(); }",
        "1\n9\n",
    );
}

#[test]
fn dfr_93_defer_after_loop_runs_after_loop_defers() {
    assert_dfr_runs(
        "fn main() { for i in 0..2 { defer println(i); } defer println(9); println(8); }",
        "0\n1\n8\n9\n",
    );
}

#[test]
fn dfr_94_for_break_outer_scope_defer_later() {
    assert_dfr_runs(
        "fn main() { defer println(9); for i in 0..3 { defer println(i); break; } println(8); }",
        "0\n8\n9\n",
    );
}

#[test]
fn dfr_95_continue_flushes_before_loop_increment_visible() {
    assert_dfr_runs(
        "fn main() { for i in 0..2 { defer println(i); continue; } println(9); }",
        "0\n1\n9\n",
    );
}

#[test]
fn dfr_96_result_ok_payload_return_value_computed_first() {
    assert_dfr_runs_gc_stress(
        "fn cleanup() { println(8); }\nfn f() -> Result<String, String> { let s = \"a\" + \"b\"; defer cleanup(); return Result::Ok(s + \"!\"); }\nfn main() { match f() { Result::Ok(v) => println(v), Result::Err(e) => println(e), } }",
        "8\nab!\n",
    );
}

#[test]
fn dfr_97_defer_argument_binary_expression_captured() {
    assert_dfr_runs(
        "fn c(n: i64) { println(n); }\nfn main() { let mut n = 1; defer c(n + 4); n = 9; println(n); }",
        "9\n5\n",
    );
}

#[test]
fn dfr_98_defer_method_receiver_expression_captured_once() {
    assert_dfr_runs(
        "class C { pub v: i64; pub fn show(self) { println(self.v); } }\nfn make(n: i64) -> C { println(n); return new C(n); }\nfn main() { defer make(1).show(); println(2); }",
        "1\n2\n1\n",
    );
}

#[test]
fn dfr_99_constructor_field_visible_to_defer() {
    assert_dfr_runs(
        "class C { pub v: i64; pub init(self, v: i64) { self.v = v; defer self.show(); println(1); } pub fn show(self) { println(self.v); } }\nfn main() { let c = new C(5); }",
        "1\n5\n",
    );
}

#[test]
fn dfr_100_reference_method_arg_rejected() {
    assert_dfr_compile_error(
        "class C { pub fn set(self, n: &mut i64) {} }\nfn main() { let c = new C(); let mut n = 1; defer c.set(&n); }",
        "reference arguments",
    );
}

#[test]
fn defer_result_01_plain_err_is_ignored() {
    assert_dfr_runs(
        &format!("{DEFER_RESULT_HELPER}\nfn main() {{ defer cleanup(false); println(\"body\"); }}"),
        "body\n",
    );
}

#[test]
fn defer_result_02_plain_ok_is_ignored() {
    assert_dfr_runs(
        &format!("{DEFER_RESULT_HELPER}\nfn main() {{ defer cleanup(true); println(\"body\"); }}"),
        "body\n",
    );
}

#[test]
fn defer_result_03_cleanup_err_does_not_replace_parent_result() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn work() -> Result<i64, String> {{ defer cleanup(false); return Ok(7); }}\nfn main() {{ match work() {{ Ok(value) => println(value), Err(error) => println(error), }} }}"
        ),
        "7\n",
    );
}

#[test]
fn defer_result_04_match_handles_err_after_body() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn main() {{ defer match cleanup(false) {{ Ok(_) => println(\"ok\"), Err(error) => println(error), }} println(\"body\"); }}"
        ),
        "body\ncleanup failed\n",
    );
}

#[test]
fn defer_result_05_match_handles_ok() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn main() {{ defer match cleanup(true) {{ Ok(_) => println(\"ok\"), Err(error) => println(error), }} println(\"body\"); }}"
        ),
        "body\nok\n",
    );
}

#[test]
fn defer_result_06_block_binds_and_matches_result() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn main() {{ defer {{ let result = cleanup(false); match result {{ Ok(_) => println(\"ok\"), Err(error) => println(error), }} }} println(\"body\"); }}"
        ),
        "body\ncleanup failed\n",
    );
}

#[test]
fn defer_result_07_match_value_is_discarded() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn main() {{ defer match cleanup(false) {{ Ok(_) => 1, Err(_) => 2, }} println(\"body\"); }}"
        ),
        "body\n",
    );
}

#[test]
fn defer_result_08_mixed_forms_share_lifo_order() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn main() {{ defer println(1); defer match cleanup(false) {{ Ok(_) => println(2), Err(_) => println(3), }} defer {{ println(4); }} println(0); }}"
        ),
        "0\n4\n3\n1\n",
    );
}

#[test]
fn defer_result_09_direct_call_argument_still_evaluates_at_registration() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn mark() -> bool {{ println(\"register\"); return false; }}\nfn main() {{ defer cleanup(mark()); println(\"body\"); }}"
        ),
        "register\nbody\n",
    );
}

#[test]
fn defer_result_10_match_scrutinee_is_evaluated_at_exit() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn run() -> Result<void, String> {{ println(\"cleanup\"); return cleanup(false); }}\nfn main() {{ defer match run() {{ Ok(_) => {{}}, Err(error) => println(error), }} println(\"body\"); }}"
        ),
        "body\ncleanup\ncleanup failed\n",
    );
}

#[test]
fn defer_result_11_direct_return_is_e0905() {
    assert_dfr_compile_error(
        &format!("{DEFER_RESULT_HELPER}\nfn main() {{ defer return cleanup(false); }}"),
        "E0905",
    );
}

#[test]
fn defer_result_12_block_return_is_e0905() {
    assert_dfr_compile_error(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn work() -> Result<void, String> {{ defer {{ return cleanup(false); }} return Ok(); }}"
        ),
        "E0905",
    );
}

#[test]
fn defer_result_13_match_arm_return_is_e0905() {
    assert_dfr_compile_error(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn work() -> Result<void, String> {{ defer match cleanup(false) {{ Ok(_) => {{}}, Err(_) => return cleanup(true), }} return Ok(); }}"
        ),
        "E0905",
    );
}

#[test]
fn defer_result_14_block_try_is_e0905() {
    assert_dfr_compile_error(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn work() -> Result<void, String> {{ defer {{ cleanup(false)?; }} return Ok(); }}"
        ),
        "E0905",
    );
}

#[test]
fn defer_result_15_match_scrutinee_try_is_e0905() {
    assert_dfr_compile_error(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn work() -> Result<void, String> {{ defer match cleanup(false)? {{ _ => {{}} }} return Ok(); }}"
        ),
        "E0905",
    );
}

#[test]
fn defer_result_16_break_is_e0905_inside_outer_loop() {
    assert_dfr_compile_error("fn main() { for _ in 0..1 { defer { break; } } }", "E0905");
}

#[test]
fn defer_result_17_continue_is_e0905_inside_outer_loop() {
    assert_dfr_compile_error(
        "fn main() { for _ in 0..1 { defer { continue; } } }",
        "E0905",
    );
}

#[test]
fn defer_result_18_await_is_e0905() {
    assert_dfr_compile_error(
        "async fn value() -> i64 { return 1; }\nasync fn main() { defer { let n = await value(); println(n); } }",
        "E0905",
    );
}

#[test]
fn defer_result_19_nested_async_cleanup_call_is_e0905() {
    assert_dfr_compile_error(
        "async fn cleanup() {}\nfn main() { defer { cleanup(); } }",
        "E0905",
    );
}

#[test]
fn defer_result_20_async_normal_exit_and_cancel_run_once() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nasync fn normal() {{ defer {{ match cleanup(false) {{ Ok(_) => {{}}, Err(_) => println(\"normal cleanup\"), }} }} }}\nasync fn waiting() {{ defer {{ match cleanup(false) {{ Ok(_) => {{}}, Err(_) => println(\"cancel cleanup\"), }} }} await sleep(5000); }}\nasync fn main() {{ await normal(); let task = waiting(); await sleep(20); task.cancel(); await sleep(50); println(task.is_cancelled()); }}"
        ),
        "normal cleanup\ncancel cleanup\ntrue\n",
    );
}

#[test]
fn defer_result_21_async_cancel_body_reads_lexical_value() {
    assert_dfr_runs(
        "async fn waiting() { let label = \"cleanup\" + \" value\"; defer { println(label); } await sleep(5000); }\nasync fn main() { let task = waiting(); await sleep(20); task.cancel(); await sleep(50); println(task.is_cancelled()); }",
        "cleanup value\ntrue\n",
    );
}

#[test]
fn defer_result_22_direct_call_argument_try_is_e0905() {
    assert_dfr_compile_error(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn value() -> Result<bool, String> {{ return Ok(false); }}\nfn work() -> Result<void, String> {{ defer cleanup(value()?); return Ok(); }}"
        ),
        "E0905",
    );
}
