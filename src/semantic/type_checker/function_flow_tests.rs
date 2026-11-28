use super::*;

fn check(source: &str) -> Vec<Diagnostic> {
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let mut checker = TypeChecker::new();
    checker.check_program(&program);
    checker.errors
}
const PRELUDE: &str = "fn safe(x: i64) -> i64 { return x; } fn slow(x: i64) -> i64 { let ch=Channel<i64>::new(); return ch.recv(); }";
#[test]
fn function_flow_mutable_reference_write() {
    reject(
        "fn replace(f: &mut fn(i64)->i64) { f=slow; }",
        "let mut f=safe; replace(&f);",
        "f(1);",
    );
}
#[test]
fn function_flow_unused_callback_argument() {
    accept(
        "fn ignore(f:fn(i64)->i64)->i64 { return 1; }",
        "let op=ignore;",
        "op(slow);",
    );
}
#[test]
fn function_flow_storing_callback_is_not_calling() {
    accept(
        "class Holder { pub f:fn(i64)->i64; } enum Maybe { Some(fn(i64)->i64), None }",
        "",
        "let h=new Holder(slow); let a=Maybe::Some(slow); let f=safe; f(1);",
    );
}
fn source(extra: &str, before: &str, body: &str) -> String {
    format!(
        "import std::collections::Array; {PRELUDE} {extra} async fn main() {{ let unused=slow; {before} let m=Mutex::new(0); lock m as v {{ {body} }} }}"
    )
}
fn accept(extra: &str, before: &str, body: &str) {
    let source = source(extra, before, body);
    let errors = check(&source);
    assert!(
        !errors.iter().any(|d| d.severity == Severity::Error),
        "{source}\n{errors:#?}"
    );
}
fn reject(extra: &str, before: &str, body: &str) {
    let source = source(extra, before, body);
    let errors = check(&source);
    assert!(
        errors
            .iter()
            .any(|d| d.code == ErrorCode::E2604 && d.labels.len() >= 2),
        "{source}\n{errors:#?}"
    );
}
#[test]
fn function_flow_local_safe() {
    accept("", "let f=safe;", "f(1);");
}
#[test]
fn function_flow_local_wait() {
    reject("", "let f=slow;", "f(1);");
}
#[test]
fn function_flow_parameter_safe() {
    accept(
        "fn apply(f: fn(i64)->i64) -> i64 { return f(1); }",
        "",
        "apply(safe);",
    );
}
#[test]
fn function_flow_parameter_wait() {
    reject(
        "fn apply(f: fn(i64)->i64) -> i64 { return f(1); }",
        "",
        "apply(slow);",
    );
}
#[test]
fn function_flow_return_safe() {
    accept(
        "fn make()->fn(i64)->i64 { return safe; }",
        "let f=make();",
        "f(1);",
    );
}
#[test]
fn function_flow_return_wait() {
    reject(
        "fn make()->fn(i64)->i64 { return slow; }",
        "let f=make();",
        "f(1);",
    );
}
#[test]
fn function_flow_field_safe() {
    accept(
        "class Holder { pub f: fn(i64)->i64; }",
        "let h=new Holder(safe); let f=h.f;",
        "f(1);",
    );
}
#[test]
fn function_flow_field_wait() {
    reject(
        "class Holder { pub f: fn(i64)->i64; }",
        "let h=new Holder(slow); let f=h.f;",
        "f(1);",
    );
}
#[test]
fn function_flow_reassignment_wait() {
    reject("", "let mut f=safe; f=slow;", "f(1);");
}
#[test]
fn function_flow_branch_wait() {
    reject("", "let f=true ? safe : slow;", "f(1);");
}
#[test]
fn function_flow_branch_safe() {
    accept("", "let f=true ? safe : safe;", "f(1);");
}
#[test]
fn function_flow_shadow_safe() {
    accept("", "let f=safe; if true { let f=slow; }", "f(1);");
}
#[test]
fn function_flow_capture_safe() {
    accept("", "let f=safe; let g=|x:i64| f(x);", "g(1);");
}
#[test]
fn function_flow_capture_wait() {
    reject("", "let f=slow; let g=|x:i64| f(x);", "g(1);");
}
#[test]
fn function_flow_lambda_safe() {
    accept("", "let f=|x:i64| x;", "f(1);");
}
#[test]
fn function_flow_lambda_wait() {
    reject(
        "",
        "let f=|x:i64| { let ch=Channel<i64>::new(); return ch.recv(); };",
        "f(1);",
    );
}
#[test]
fn function_flow_chain_safe() {
    accept(
        "fn apply(f:fn(i64)->i64)->i64 { return f(1); } fn relay(f:fn(i64)->i64)->i64 { return apply(f); }",
        "",
        "relay(safe);",
    );
}
#[test]
fn function_flow_chain_wait() {
    reject(
        "fn apply(f:fn(i64)->i64)->i64 { return f(1); } fn relay(f:fn(i64)->i64)->i64 { return apply(f); }",
        "",
        "relay(slow);",
    );
}
#[test]
fn function_flow_indirect_higher_order_safe() {
    accept(
        "fn apply(f:fn(i64)->i64)->i64 { return f(1); }",
        "let a=apply;",
        "a(safe);",
    );
}
#[test]
fn function_flow_indirect_higher_order_wait() {
    reject(
        "fn apply(f:fn(i64)->i64)->i64 { return f(1); }",
        "let a=apply;",
        "a(slow);",
    );
}
#[test]
fn function_flow_return_parameter_safe() {
    accept(
        "fn identity(f:fn(i64)->i64)->fn(i64)->i64 { return f; }",
        "let f=identity(safe);",
        "f(1);",
    );
}
#[test]
fn function_flow_return_parameter_wait() {
    reject(
        "fn identity(f:fn(i64)->i64)->fn(i64)->i64 { return f; }",
        "let f=identity(slow);",
        "f(1);",
    );
}
#[test]
fn function_flow_array_safe() {
    accept("", "let a=[safe]; let f=a[0];", "f(1);");
}
#[test]
fn function_flow_array_wait() {
    reject("", "let a=[slow]; let f=a[0];", "f(1);");
}
#[test]
fn function_flow_array_alias_write() {
    reject("", "let a=[safe]; let b=a; b[0]=slow; let f=a[0];", "f(1);");
}
#[test]
fn function_flow_field_alias_write() {
    reject(
        "class Holder { pub f: fn(i64)->i64; }",
        "let h=new Holder(safe); let alias=h; alias.f=slow; let f=h.f;",
        "f(1);",
    );
}
#[test]
fn function_flow_explicit_constructor_safe() {
    accept(
        "class Holder { pub f:fn(i64)->i64; pub init(self,f:fn(i64)->i64) { self.f=f; } }",
        "let h=new Holder(safe); let f=h.f;",
        "f(1);",
    );
}
#[test]
fn function_flow_full_type_fallback() {
    let errors = check(
        "fn safe(x:i64)->i64 { return x; } fn slow(x:Channel<i64>)->i64 { return x.recv(); } pub fn apply(f:fn(i64)->i64)->i64 { return f(1); } async fn main() { let unused=slow; let m=Mutex::new(0); lock m as v { apply(safe); } }",
    );
    assert!(
        !errors.iter().any(|d| d.severity == Severity::Error),
        "{errors:#?}"
    );
}

#[test]
fn function_flow_public_return_escapes_callback_parameters() {
    reject(
        "pub fn exported()->fn(fn(i64)->i64)->i64 { return apply; } fn apply(f:fn(i64)->i64)->i64 { return f(1); }",
        "",
        "apply(safe);",
    );
}
#[test]
fn function_flow_private_method_parameter_safe() {
    accept(
        "class Worker { fn apply(self,f:fn(i64)->i64)->i64 { return f(1); } pub fn go(self)->i64 { return self.apply(safe); } }",
        "let w=new Worker();",
        "w.go();",
    );
}
#[test]
fn function_flow_private_method_parameter_wait() {
    reject(
        "class Worker { fn apply(self,f:fn(i64)->i64)->i64 { return f(1); } pub fn go(self)->i64 { return self.apply(slow); } }",
        "let w=new Worker();",
        "w.go();",
    );
}

#[test]
fn function_flow_addressable_builtin_safe() {
    accept("", "let f=pow;", "f(2.0,3.0);");
}

#[test]
fn function_flow_reference_places_safe_and_waiting() {
    for (extra, setup, place, read) in [
        ("", "let a=[safe];", "a[0]", "a[0]"),
        ("", "let a=[safe]; let b=a;", "b[0]", "a[0]"),
        ("", "let a=[[safe]];", "a[0][0]", "a[0][0]"),
        (
            "class Holder { pub f:fn(i64)->i64; }",
            "let mut k=0; let a=new Holder(|x:i64| { k=k+1; return x; }); let b=a;",
            "b.f",
            "a.f",
        ),
        (
            "class Holder { pub f:Array<fn(i64)->i64>; }",
            "let a=new Holder([safe]);",
            "a.f[0]",
            "a.f[0]",
        ),
    ] {
        for target in ["safe", "slow"] {
            let helper = format!("{extra} fn replace(f:&mut fn(i64)->i64) {{ f={target}; }}");
            let before = format!("{setup} replace(&{place});");
            let body = format!("let f={read}; f(1);");
            if target == "safe" {
                accept(&helper, &before, &body);
            } else {
                reject(&helper, &before, &body);
            }
        }
    }
}

#[test]
fn function_flow_super_constructor_safe_and_waiting() {
    for base in [
        "open class Base { pub f:fn(i64)->i64; pub init(self,f:fn(i64)->i64) { self.f=f; } }",
        "open class Base { pub f:fn(i64)->i64; }",
    ] {
        for target in ["safe", "slow"] {
            let classes = format!(
                "{base} open class Middle extends Base {{ pub init(self,f:fn(i64)->i64) {{ super.init(f); }} }} class Child extends Middle {{ pub init(self,f:fn(i64)->i64) {{ super.init(f); }} }}"
            );
            let before = format!("let a=new Base(safe); let b=new Child({target});");
            let body = "let f=b.f; f(1);";
            if target == "safe" {
                accept(&classes, &before, body);
            } else {
                reject(&classes, &before, body);
            }
        }
    }
}

#[test]
fn function_flow_super_constructor_reference_write() {
    reject(
        "open class Base { pub init(self, f:&mut fn(i64)->i64) { f=slow; } } class Child extends Base { pub init(self, f:&mut fn(i64)->i64) { super.init(&f); } }",
        "let a=[safe]; let b=new Child(&a[0]);",
        "let f=a[0]; f(1);",
    );
}

#[test]
fn function_flow_checked_memberwise_layout_matches_argument_order() {
    for depth in [8, 32, 128] {
        let mut source="fn safe(x:i64)->i64 { return x; } open class Base { pub callback:fn(i64)->i64; pub marker:i64; }".to_owned();
        let mut base = "Base".to_owned();
        for index in 0..depth {
            source.push_str(&format!("open class C{index} extends {base} {{}}"));
            base = format!("C{index}");
        }
        source.push_str(&format!(
            "class Leaf extends {base} {{ pub tail:bool; }} fn main() {{"
        ));
        for index in 0..depth {
            source.push_str(&format!("let value{index}=new Leaf(safe,1,true); let cb{index}=value{index}.callback; cb{index}(1);"));
        }
        source.push('}');
        let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut checker = TypeChecker::new();
        checker.effect_graph_only = Some(crate::semantic::call_graph::CallGraph::default());
        checker.check_program(&program);
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        assert_eq!(checker.indirect_lock.memberwise.len(), depth);
        for (_, fields) in &checker.indirect_lock.memberwise {
            assert_eq!(fields, &["callback", "marker", "tail"]);
        }
        let slots: usize = checker
            .indirect_lock
            .memberwise
            .iter()
            .map(|(_, fields)| fields.len())
            .sum();
        assert_eq!(slots, 3 * depth);
        println!("memberwise-layout depth={depth} calls={depth} recorded_slots={slots}");
    }
}
