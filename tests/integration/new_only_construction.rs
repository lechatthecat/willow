//! Objects are created only with `new`. A call spelled like a constructor is
//! rejected with E0843 and a fix that keeps the written qualifier and type
//! arguments; enum variants, factories, function values and callable fields
//! keep their ordinary call meaning.
use super::support::*;

const MODEL: &str = r#"
pub class User { pub score: i64; }
pub class Box<T> { pub value: T; }
pub class Counter { pub n: i64; pub init(self, n: i64) { self.n = n + 1; } }
"#;

fn single_file_error(source: &str) -> String {
    let error = compile_error_stderr(source);
    assert!(error.contains("error[E0843]"), "{source}: {error}");
    error
}

fn project_error(main: &str) -> String {
    let error =
        compile_temp_project_error_stderr(&[("main.wi", main), ("model.wi", MODEL)], "main.wi");
    assert!(error.contains("error[E0843]"), "{main}: {error}");
    error
}

#[test]
fn new_only_call_form_is_rejected_with_written_spelling() {
    let local = "class User { pub score: i64; } class Box<T> { pub value: T; } \
                 class Counter { pub n: i64; pub init(self, n: i64) { self.n = n; } } \
                 class Empty {}";
    for (call, class, help) in [
        // Memberwise, explicit-init and zero-argument classes.
        ("User(1)", "User", "new User(...)"),
        ("Counter(1)", "Counter", "new Counter(...)"),
        ("Empty()", "Empty", "new Empty(...)"),
        // Generic classes, explicit and inferred.
        ("Box<i64>(1)", "Box", "new Box<i64>(...)"),
        ("Box(1)", "Box", "new Box(...)"),
        ("Box<Box<i64>>(new Box(1))", "Box", "new Box<Box<i64>>(...)"),
    ] {
        let error = single_file_error(&format!("{local} fn main() {{ let v = {call}; }}"));
        assert!(
            error.contains(&format!(
                "class `{class}` can only be constructed with `new`"
            )),
            "{call}: {error}"
        );
        assert!(
            error.contains(&format!("write `{help}` instead")),
            "{call}: {error}"
        );
        assert_eq!(error.matches("error[").count(), 1, "{call}: {error}");
    }
}

#[test]
fn new_only_qualified_and_aliased_call_forms_keep_their_qualifier() {
    for (call, help) in [
        ("model::User(1)", "new model::User(...)"),
        ("m::User(1)", "new m::User(...)"),
        ("Person(1)", "new Person(...)"),
        ("model::Box<i64>(1)", "new model::Box<i64>(...)"),
        ("m::Box(1)", "new m::Box(...)"),
        ("Container<i64>(1)", "new Container<i64>(...)"),
        ("model::Counter(1)", "new model::Counter(...)"),
    ] {
        let main = format!(
            "import model;\nimport model as m;\nimport model::User as Person;\n\
             import model::Box as Container;\nfn main() {{ let v = {call}; }}\n"
        );
        let error = project_error(&main);
        assert!(
            error.contains(&format!("write `{help}` instead")),
            "{call}: {error}"
        );
        assert_eq!(error.matches("error[").count(), 1, "{call}: {error}");
    }
}

#[test]
fn new_only_call_form_in_every_expression_position() {
    let classes = "class User { pub score: i64; } class Box<T> { pub value: T; }";
    for body in [
        // Contextual expected types use the contextual checking path.
        "fn main() { let u: User = User(1); }",
        "fn main() { let b: Box<i64> = Box(1); }",
        "fn f() -> User { User(1) } fn main() {}",
        "fn f() -> User { return User(1); } fn main() {}",
        "fn take(u: User) {} fn main() { take(User(1)); }",
        "fn main() { let xs: Array<User> = [User(1)]; }",
        "fn main() { let o: Option<User> = Some(User(1)); }",
        "fn main() { let u = true ? User(1) : new User(2); }",
        "fn main() { let u = match 1 { 1 => User(1), _ => new User(2) }; }",
        "fn main() { let b = new Box(User(1)); }",
        "fn main() { let f = || User(1); }",
        "fn main() { defer User(1); }",
        "fn main() { defer { User(1); } }",
        "class Holder { pub user: User; pub init(self) { self.user = User(1); } } fn main() {}",
    ] {
        let error = single_file_error(&format!("{classes} {body}"));
        assert!(error.contains("`new` is missing"), "{body}: {error}");
        assert_eq!(error.matches("error[E0843]").count(), 1, "{body}: {error}");
    }
}

#[test]
fn new_only_qualified_contextual_call_form_is_rejected() {
    // A qualified call with an expected type is checked before the ordinary
    // path; it must reach the same diagnostic.
    let error = project_error(
        "import model;\nfn main() { let b: model::Box<i64> = model::Box(1); \
         let u: model::User = model::User(2); }\n",
    );
    assert_eq!(error.matches("error[E0843]").count(), 2, "{error}");
}

#[test]
fn new_only_call_form_checks_arguments_without_cascades() {
    // Argument errors still surface; the missing `new` does not cascade into
    // the binding's later uses.
    let classes = "class User { pub score: i64; }";
    let error = compile_error_stderr(&format!(
        "{classes} fn main() {{ let u = User(missing); println(u.score + 1); }}"
    ));
    assert!(error.contains("error[E0843]"), "{error}");
    assert!(error.contains("missing"), "{error}");
    assert_eq!(error.matches("error[").count(), 2, "{error}");
    let error = compile_error_stderr(&format!(
        "{classes} fn main() {{ let u = User(1); println(u.score + 1); u.nothing(); }}"
    ));
    assert_eq!(error.matches("error[").count(), 1, "{error}");
}

#[test]
fn new_only_reports_each_call_form_site_once() {
    for count in [1, 8, 64] {
        let body: String = (0..count)
            .map(|i| format!("let v{i} = User({i}); "))
            .collect();
        let error = compile_error_stderr(&format!(
            "class User {{ pub score: i64; }} fn main() {{ {body} }}"
        ));
        assert_eq!(error.matches("error[E0843]").count(), count, "{error}");
    }
}

#[test]
fn new_only_subclass_call_form_is_rejected() {
    let error = single_file_error(
        "class Base { pub n: i64; pub init(self, n: i64) { self.n = n; } } \
         class Sub extends Base { pub init(self, n: i64) { super.init(n); } } \
         fn main() { let s = Sub(1); }",
    );
    assert!(error.contains("write `new Sub(...)` instead"), "{error}");
}

#[test]
fn new_only_construction_runs_in_every_spelling() {
    let main = r#"
import model;
import model as m;
import model::User as Person;
import model::Box as Container;
class Local { pub n: i64; }
class Pair<A, B> { pub a: A; pub b: B; }
fn make() -> Local { new Local(7) }
fn main() {
    println(new model::User(1).score);
    println(new m::User(2).score);
    println(new Person(3).score);
    println(new model::Box<i64>(4).value);
    println(new m::Box(5).value);
    println(new Container<i64>(6).value);
    println(make().n);
    let pair = new Pair(8, true);
    println(pair.a);
    let typed: m::Box<i64> = new m::Box(9);
    println(typed.value);
    println(new model::Counter(9).n);
    let nested = new Container(new Container(11));
    println(nested.value.value);
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("main.wi", main), ("model.wi", MODEL)], "main.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n");
}

#[test]
fn new_only_ordinary_calls_keep_their_meaning() {
    // Enum variants, static factories, function values, callable fields,
    // shadowing bindings and functions sharing a class name stay calls.
    let source = r#"
enum Shape { Circle(i64), Square(i64) }
class User {
    pub score: i64;
    pub static fn create(score: i64) -> User { new User(score * 10) }
}
class Holder { pub callback: fn(i64) -> i64; }
class Twin { pub n: i64; }
fn Twin(n: i64) -> i64 { n + 100 }
fn double(x: i64) -> i64 { x * 2 }
fn apply(f: fn(i64) -> i64, x: i64) -> i64 { f(x) }
fn area(shape: Shape) -> i64 {
    match shape { Shape::Circle(r) => r * 3, Shape::Square(s) => s * s }
}
fn main() {
    let some: Option<i64> = Some(1);
    let ok: Result<i64, String> = Ok(2);
    println(some.unwrap() + ok.unwrap());
    println(area(Shape::Circle(2)) + area(Shape::Square(3)));
    println(User::create(4).score);
    let f = double;
    println(f(5));
    println(apply(double, 6));
    let holder = new Holder(|x: i64| x + 1);
    let callback = holder.callback;
    println(callback(7));
    let User = |x: i64| x - 1;
    println(User(9));
    println(Twin(1));
    println(new Twin(2).n);
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "3\n15\n40\n10\n12\n8\n8\n101\n2\n");
}
