//! Willow generics have no interface bounds. The CLI explains the limit for a
//! written bound, a `where` clause and a method call on a type parameter, and
//! the suggested alternatives run.
use super::support::*;

const SHOW: &str = "import std::collections::Array;
interface Show { fn show(self) -> String; }
class N implements Show { pub v: i64; pub fn show(self) -> String { return \"n\"; } }
";

fn check_stderr(name: &str, body: &str) -> String {
    let project = TestProject::new(name, &[("main.wi", &format!("{SHOW}{body}"))]);
    let output = project.check("main.wi");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!output.status.success(), "{stderr}");
    stderr
}

#[test]
fn bounds_and_where_clauses_explain_the_limit() {
    for (name, body, written) in [
        (
            "bound_inline",
            "fn join<T: Show>(xs: Array<T>) -> i64 { return xs.len(); }\nfn main() { println(join([new N(1)])); }\n",
            "`T: Show`",
        ),
        (
            "bound_where",
            "fn join<T>(xs: Array<T>) -> i64 where T: Show { return xs.len(); }\nfn main() { println(join([new N(1)])); }\n",
            "`where T: Show`",
        ),
    ] {
        let stderr = check_stderr(name, body);
        assert_eq!(stderr.matches("error[").count(), 1, "{stderr}");
        assert!(
            stderr.contains(&format!(
                "error[E0102]: type parameter bounds are not supported: {written}"
            )),
            "{stderr}"
        );
        assert!(stderr.contains("`x: Show` or `Array<Show>`"), "{stderr}");
        assert!(stderr.contains("`op: fn(T) -> String`"), "{stderr}");
    }
}

#[test]
fn method_call_on_type_parameter_names_both_alternatives() {
    let stderr = check_stderr(
        "bound_method_call",
        "fn join<T>(xs: Array<T>) -> String { let mut s = \"\"; for x in xs { s = s + x.show(); } return s; }\nfn main() { println(join([new N(1)])); }\n",
    );
    assert!(
        stderr.contains("error[E0201]: type parameter `T` has no method `show`"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Willow has no interface bounds"),
        "{stderr}"
    );
    assert!(stderr.contains("take `Show` instead of `T`"), "{stderr}");
    assert!(stderr.contains("`show: fn(T) -> String`"), "{stderr}");
}

#[test]
fn generic_interface_alternatives_example_runs() {
    let (out, ok) = compile_and_run(include_str!(
        "../../example/generic_interface_alternatives.wi"
    ));
    assert!(ok, "{out}");
    assert_eq!(out, "(1,2) willow \n(3,4) \n10 20 30 \n");
}
