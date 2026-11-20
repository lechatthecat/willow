//! Public builtin naming (snake_case, retired camelCase names diagnosed with
//! the current spelling) and `String.find` returning `Option<i64>` instead of
//! a `-1` sentinel.

use super::check_source;
use crate::semantic::intrinsics::renamed_method;

/// `check_source` sees no prelude: declare the Option enum patterns name.
const PRELUDE: &str = "import std::collections::{Array, Map}; enum Option<T> { Some(T), None }";

/// Error diagnostics of `source` (unused-import warnings are irrelevant here).
fn check_errors(source: &str) -> Vec<crate::diagnostics::Diagnostic> {
    check_source(source)
        .into_iter()
        .filter(|d| d.severity == crate::diagnostics::Severity::Error)
        .collect()
}

fn check_body(params: &str, body: &str) -> Vec<crate::diagnostics::Diagnostic> {
    check_errors(&format!("{PRELUDE} fn test({params}) {{ {body} }}"))
}

fn rename_diagnostics(diagnostics: &[crate::diagnostics::Diagnostic]) -> usize {
    diagnostics
        .iter()
        .filter(|d| d.message.contains("it was renamed to `to_string`"))
        .count()
}

#[test]
fn renamed_method_table_maps_retired_names_only() {
    assert_eq!(renamed_method("toString"), Some("to_string"));
    for current in ["to_string", "len", "find", "tostring", "ToString", ""] {
        assert_eq!(renamed_method(current), None, "{current}");
    }
}

#[test]
fn retired_to_string_names_new_method_on_every_builtin_receiver() {
    for (param, receiver, ty) in [
        ("n: i64", "n", "i64"),
        ("f: f64", "f", "f64"),
        ("b: bool", "b", "bool"),
        ("s: String", "s", "String"),
        ("a: Array<i64>", "a", "Array<i64>"),
        ("m: Map<String, i64>", "m", "Map<String, i64>"),
    ] {
        let diagnostics = check_body(param, &format!("{receiver}.toString();"));
        assert_eq!(diagnostics.len(), 1, "{ty}: {diagnostics:?}");
        let d = &diagnostics[0];
        assert_eq!(
            d.message,
            format!("no method `toString` on type `{ty}`; it was renamed to `to_string`")
        );
        assert_eq!(d.code, crate::diagnostics::ErrorCode::E0201, "{d:?}");
        assert!(
            d.helps
                .iter()
                .any(|h| h.contains("snake_case") && h.contains(".to_string(...)")),
            "{d:?}"
        );
        assert!(
            d.labels.iter().any(|l| l.message == "old method name"),
            "{d:?}"
        );
    }
}

#[test]
fn retired_name_on_literal_receivers() {
    for receiver in ["1", "2.5", "true", "\"x\"", "[1, 2]"] {
        let diagnostics = check_body("", &format!("{receiver}.toString();"));
        assert_eq!(
            rename_diagnostics(&diagnostics),
            1,
            "{receiver}: {diagnostics:?}"
        );
        assert_eq!(diagnostics.len(), 1, "{receiver}: {diagnostics:?}");
    }
}

#[test]
fn retired_name_keeps_checking_the_call_as_the_new_method() {
    // The recovered call types as `String`, so chained use reports nothing more.
    let diagnostics = check_body("n: i64", "let s: String = n.toString(); s.len();");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let diagnostics = check_body("n: i64", "let len: i64 = n.toString().len();");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let diagnostics = check_body("n: i64", "let s = \"#\" + n.toString();");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    // A wrong-typed use of the recovered String is still reported.
    let diagnostics = check_body("n: i64", "let wrong: i64 = n.toString();");
    assert_eq!(rename_diagnostics(&diagnostics), 1, "{diagnostics:?}");
    assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
}

#[test]
fn retired_name_with_bad_arity_reports_rename_and_arity() {
    let diagnostics = check_body("n: i64", "n.toString(2);");
    assert_eq!(rename_diagnostics(&diagnostics), 1, "{diagnostics:?}");
    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("`to_string()` takes no arguments")),
        "{diagnostics:?}"
    );
}

#[test]
fn user_declared_retired_name_stays_valid() {
    let source = "class Point { pub x: i64; \
        pub fn toString(self) -> String { return \"p\"; } } \
        fn show(p: Point) -> String { return p.toString(); }";
    let diagnostics = check_errors(source);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn user_class_with_new_name_gets_rename_diagnostic() {
    let source = "class Point { pub x: i64; \
        pub fn to_string(self) -> String { return \"p\"; } } \
        fn show(p: Point) -> String { return p.toString(); }";
    let diagnostics = check_errors(source);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0].message,
        "no method `toString` on type `Point`; it was renamed to `to_string`"
    );
}

#[test]
fn class_without_either_name_gets_plain_unknown_method() {
    let source = "class Point { pub x: i64; } fn show(p: Point) { p.toString(); }";
    let diagnostics = check_errors(source);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(rename_diagnostics(&diagnostics), 0, "{diagnostics:?}");
    assert!(
        diagnostics[0].message.contains("toString"),
        "{diagnostics:?}"
    );
}

#[test]
fn interface_receiver_with_new_name_gets_rename_diagnostic() {
    let source = "interface Show { fn to_string(self) -> String; } \
        fn show(v: Show) -> String { return v.toString(); }";
    let diagnostics = check_errors(source);
    assert_eq!(rename_diagnostics(&diagnostics), 1, "{diagnostics:?}");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
}

#[test]
fn receivers_without_to_string_get_no_rename_hint() {
    for (param, receiver) in [("o: Option<i64>", "o"), ("r: Result<i64, String>", "r")] {
        let diagnostics = check_body(param, &format!("{receiver}.toString();"));
        assert!(!diagnostics.is_empty(), "{receiver}");
        assert_eq!(rename_diagnostics(&diagnostics), 0, "{diagnostics:?}");
    }
}

#[test]
fn other_misspellings_get_no_rename_hint() {
    for method in ["tostring", "ToString", "toStrng", "to_String"] {
        let diagnostics = check_body("n: i64", &format!("n.{method}();"));
        assert_eq!(diagnostics.len(), 1, "{method}: {diagnostics:?}");
        assert_eq!(rename_diagnostics(&diagnostics), 0, "{diagnostics:?}");
    }
}

#[test]
fn current_name_is_accepted_on_every_builtin_receiver() {
    let diagnostics = check_body(
        "n: i64, f: f64, b: bool, s: String, a: Array<i64>, m: Map<String, i64>",
        "let all: String = n.to_string() + f.to_string() + b.to_string() \
         + s.to_string() + a.to_string() + m.to_string();",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn rename_diagnostics_scale_linearly_with_old_calls() {
    for count in [1, 8, 64] {
        let body = "n.toString();".repeat(count);
        let diagnostics = check_body("n: i64", &body);
        assert_eq!(diagnostics.len(), count);
        assert_eq!(rename_diagnostics(&diagnostics), count);
    }
}

#[test]
fn string_find_returns_option_i64() {
    for body in [
        "let found: Option<i64> = s.find(\"b\");",
        "let at: i64 = s.find(\"b\").unwrap_or(-1);",
        "if let Some(at) = s.find(\"b\") { let x: i64 = at; }",
        "match s.find(\"b\") { Some(at) => { let x: i64 = at; }, None => {} }",
        "let missing: bool = s.find(\"z\").is_none();",
        "let present: bool = s.find(\"b\").is_some();",
        "let doubled = s.find(\"b\").map(|at| at * 2);",
    ] {
        let diagnostics = check_body("s: String", body);
        assert!(diagnostics.is_empty(), "{body}: {diagnostics:?}");
    }
}

#[test]
fn string_find_propagates_with_question_mark_in_option_function() {
    let diagnostics = check_errors(
        "enum Option<T> { Some(T), None } fn locate(s: String) -> Option<i64> { let at = s.find(\"b\")?; return Option::Some(at + 1); }",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn string_find_sentinel_uses_are_type_errors() {
    for body in [
        "let at: i64 = s.find(\"b\");",
        "if s.find(\"b\") >= 0 { }",
        "if s.find(\"b\") == -1 { }",
        "let next = s.find(\"b\") + 1;",
    ] {
        let diagnostics = check_body("s: String", body);
        assert!(!diagnostics.is_empty(), "{body}");
        assert!(
            diagnostics.iter().any(|d| d.message.contains("Option")),
            "{body}: {diagnostics:?}"
        );
    }
}

fn is_snake_case(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn is_pascal_case(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Every public item of a bundled Willow source: (name, is a type).
fn bundled_public_names(source: &str) -> Vec<(String, bool)> {
    use crate::parser::ast::Item;
    let tokens = crate::lexer::Lexer::new(source)
        .tokenize()
        .expect("bundled source lexes");
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "bundled source parses: {errors:?}");
    let mut names = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) if f.public => names.push((f.name.clone(), false)),
            Item::Function(_) => {}
            Item::Class(c) => {
                if c.public {
                    names.push((c.name.clone(), true));
                }
                names.extend(
                    c.methods
                        .iter()
                        .filter(|m| m.public)
                        .map(|m| (m.name.clone(), false)),
                );
            }
            Item::Enum(e) if e.public => {
                names.push((e.name.clone(), true));
                names.extend(e.variants.iter().map(|v| (v.name.clone(), true)));
            }
            Item::Enum(_) => {}
            Item::Interface(i) if i.public => {
                names.push((i.name.clone(), true));
                names.extend(i.methods.iter().map(|m| (m.name.clone(), false)));
            }
            Item::Interface(_) => {}
        }
    }
    names
}

/// The public naming inventory: builtin intrinsic methods, Option/Result
/// methods, namespace and static builtins, the `std` schema, and the bundled
/// Willow sources (prelude, `std::collections`, every source std module). Callable names
/// are snake_case; type and variant names are PascalCase.
#[test]
fn public_builtin_and_std_names_follow_the_naming_rule() {
    use crate::semantic::builtin_types::BuiltinTypeId as B;
    use crate::semantic::intrinsics::{Intrinsic, builtin_static_constructor, namespace_builtin};
    use crate::stdlib_schema::{STDLIB_SCHEMA, StdItemKind};

    let mut checked = 0usize;
    let mut violations = Vec::new();
    let mut check = |name: &str, is_type: bool, origin: &str| {
        checked += 1;
        let ok = if is_type {
            is_pascal_case(name)
        } else {
            is_snake_case(name)
        };
        if !ok {
            violations.push(format!("{origin}: {name}"));
        }
    };

    for intrinsic in Intrinsic::ALL {
        for name in intrinsic.source_names() {
            // `__abi_*` names are compiler-internal hooks used only by the
            // bundled `std` sources, never public API.
            if !name.starts_with("__abi_") {
                check(name, false, "intrinsic method");
            }
        }
    }
    for (owner, method) in [
        (B::Option, "is_some"),
        (B::Option, "is_none"),
        (B::Result, "is_ok"),
        (B::Result, "is_err"),
        (B::Option, "unwrap"),
        (B::Result, "unwrap_err"),
        (B::Option, "expect"),
        (B::Option, "unwrap_or"),
        (B::Option, "map"),
        (B::Result, "map_err"),
        (B::Option, "and_then"),
        (B::Option, "or_else"),
    ] {
        assert!(
            super::builtin_methods::option_method(owner, method).is_some(),
            "{method} is an Option/Result method"
        );
        check(method, false, "Option/Result method");
    }
    for (class, method) in [
        ("i64", "abs"),
        ("i64", "min"),
        ("i64", "max"),
        ("i64", "clamp"),
        ("i64", "parse"),
        ("f64", "abs"),
        ("f64", "sqrt"),
        ("f64", "floor"),
        ("f64", "ceil"),
        ("f64", "round"),
        ("f64", "from_i64"),
        ("f64", "trunc_to_i64"),
        ("f64", "to_string"),
        ("f64", "parse"),
    ] {
        assert!(
            namespace_builtin(class, method).is_some(),
            "{class}::{method}"
        );
        check(method, false, "namespace builtin");
    }
    for (class, method) in [
        ("Map", "new"),
        ("Channel", "new"),
        ("Channel", "with_capacity"),
        ("CancellationToken", "new"),
        ("TaskScope", "new"),
        ("AtomicI64", "new"),
        ("AtomicBool", "new"),
        ("BlockingCell", "new"),
        ("BlockingRwCell", "new"),
        ("Mutex", "new"),
        ("RwLock", "new"),
    ] {
        assert!(
            builtin_static_constructor(class, method).is_some(),
            "{class}::{method}"
        );
        check(class, true, "builtin type");
        check(method, false, "static constructor");
    }
    for module in STDLIB_SCHEMA {
        check(module.name, false, "std module");
        for item in module.items {
            let is_type = !matches!(item.kind, StdItemKind::Function { .. });
            check(item.name, is_type, module.name);
            if !is_type {
                assert!(
                    namespace_builtin(module.name, item.name).is_some()
                        || matches!(module.name, "io" | "option" | "result" | "collections"),
                    "std::{}::{} resolves",
                    module.name,
                    item.name
                );
            }
        }
    }
    let bundled = [
        ("prelude", crate::prelude::PRELUDE_SOURCE),
        (
            "std::collections",
            include_str!("../../../std/collections.wi"),
        ),
    ];
    let source_modules = crate::module::std_registry::SOURCE_STD_MODULES
        .iter()
        .map(|module| (module.path, module.source));
    for (origin, source) in bundled.into_iter().chain(source_modules) {
        // `$`-prefixed items are parser-synthesized (tuple desugaring) and
        // cannot be spelled in source.
        for (name, is_type) in bundled_public_names(source)
            .into_iter()
            .filter(|(name, _)| !name.starts_with('$'))
        {
            check(&name, is_type, origin);
        }
    }
    assert!(
        violations.is_empty(),
        "naming rule violations: {violations:?}"
    );
    assert!(checked > 100, "inventory unexpectedly small: {checked}");
}
