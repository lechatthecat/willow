//! Behavioral snapshots captured before consolidating builtin checks.
use super::check_source;

#[test]
fn builtin_contract_diagnostics() {
    // Each family covers too few/many arguments, nested errors, references,
    // scalar mismatches and contextually typed closures. Hash the complete
    // ordered diagnostics (including codes, labels, spans and help), not just
    // the message count. On mismatch print every case for investigation.
    let families: &[(&str, &[&str], u64)] = &[
        (
            "Option<i64>",
            &[
                "is_some",
                "is_none",
                "unwrap",
                "expect",
                "unwrap_or",
                "map",
                "and_then",
                "or_else",
            ],
            8170932197896119529,
        ),
        (
            "Result<i64, String>",
            &[
                "is_ok",
                "is_err",
                "unwrap",
                "unwrap_err",
                "expect",
                "unwrap_or",
                "map",
                "map_err",
                "and_then",
                "or_else",
            ],
            15748745498989291629,
        ),
        (
            "Channel<i64>",
            &["send", "recv", "recv_opt", "close"],
            12931088871476261831,
        ),
        (
            "Task<i64>",
            &["result", "cancel", "is_cancelled", "join", "try_join"],
            16152542586801427613,
        ),
        (
            "CancellationToken",
            &[
                "cancel",
                "is_cancelled",
                "child",
                "finish",
                "attach",
                "add",
                "missing",
            ],
            5029321944362995253,
        ),
        (
            "TaskScope",
            &[
                "cancel",
                "is_cancelled",
                "child",
                "finish",
                "attach",
                "add",
                "missing",
            ],
            2882140490731841959,
        ),
        (
            "AtomicI64",
            &[
                "load",
                "store",
                "add",
                "sub",
                "swap",
                "compare_exchange",
                "missing",
            ],
            6179430413252869510,
        ),
        (
            "AtomicBool",
            &["load", "store", "add", "swap", "compare_exchange"],
            6135467808331348761,
        ),
        (
            "BlockingCell<i64>",
            &["get", "set", "missing"],
            13476478997889224419,
        ),
        (
            "BlockingRwCell<i64>",
            &["read", "write", "missing"],
            11736481423597076537,
        ),
        (
            "Map<String, i64>",
            &[
                "insert", "get", "remove", "contains", "keys", "values", "entries", "len",
                "freeze", "toString",
            ],
            6065699962330834382,
        ),
        (
            "Array<i64>",
            &["push", "pop", "len", "get", "set"],
            12525021924727481343,
        ),
        (
            "i64",
            &["toString", "send", "recv", "close"],
            22192759928931584,
        ),
    ];
    for (ty, methods, expected) in families {
        let mut snapshot = String::new();
        for method in *methods {
            for args in [
                "",
                "1",
                "true",
                "\"x\"",
                "1, 2",
                "unknown",
                "unknown, other",
                "&v",
                "|n: i64| n",
                "|n: bool| n",
                "|| 1",
                "|n| n",
            ] {
                let source = format!(
                    "import std::collections::{{Array, Map}}; fn test(x: {ty}, v: i64) {{ x.{method}({args}); }}"
                );
                let diagnostics = check_source(&source);
                snapshot.push_str(&format!("{method}({args}): {diagnostics:?}\n"));
            }
        }
        let hash = snapshot.bytes().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        assert_eq!(hash, *expected, "{ty}:\n{snapshot}");
    }
}

#[test]
fn builtin_contract_constructors() {
    let mut snapshot = String::new();
    for target in [
        "Map<String, i64>::new",
        "Channel<i64>::new",
        "Channel<i64>::with_capacity",
        "Mutex<i64>::new",
        "RwLock<i64>::new",
        "BlockingCell<i64>::new",
        "BlockingRwCell<i64>::new",
        "AtomicI64::new",
        "AtomicBool::new",
        "CancellationToken::new",
        "TaskScope::new",
        "f64::from_i64",
        "f64::trunc_to_i64",
        "i64::parse",
        "f64::parse",
        "f64::to_string",
    ] {
        for args in [
            "",
            "1",
            "true",
            "\"x\"",
            "1, 2",
            "unknown",
            "unknown, other",
            "&v",
        ] {
            let source =
                format!("import std::collections::Map; fn test(v: i64) {{ {target}({args}); }}");
            snapshot.push_str(&format!("{target}({args}): {:?}\n", check_source(&source)));
        }
    }
    let hash = snapshot.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    });
    assert_eq!(hash, 12800810462358245457, "{snapshot}");
}

#[test]
fn builtin_contract_work_is_linear_in_calls_and_arguments() {
    use super::builtin_methods::BUILTIN_WORK;
    for calls in [1, 8, 64] {
        for args in [1, 8, 64] {
            let arguments = vec!["missing"; args].join(",");
            let body = format!("x.unwrap({arguments});").repeat(calls);
            let source = format!("fn test(x: Option<i64>) {{ {body} }}");
            BUILTIN_WORK.with(|work| work.set((0, 0)));
            let diagnostics = check_source(&source);
            assert_eq!(diagnostics.len(), calls * (args + 1));
            assert_eq!(BUILTIN_WORK.with(|work| work.get()), (calls, calls * args));
            println!(
                "calls={calls}, arguments={args}, arity checks={calls}, discarded expression checks={}",
                calls * args
            );
        }
    }
}
