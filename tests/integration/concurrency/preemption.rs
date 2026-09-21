use super::*;

// ───────────────────────────────────────────────────────────────────────────
// E0810 for a looping method reached through a typed NON-`self` receiver
// (`obj.heavy()`), resolved by the type checker since the AST-only
// ConcurrencyAnalyzer cannot type the receiver (willow-0a6k.2).
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn test_typed_receiver_looping_method_reports_e0810() {
    assert_sync_preemption_capability(
        r#"
class Work {
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}

async fn run(w: Work) -> i64 {
    return w.heavy(10);
}
"#,
        &[
            "error[E0810]",
            "sync helper `Work::heavy` with a loop is not preemptible in task context",
            "this call can monopolize the scheduler worker",
        ],
    );
}

#[test]
fn test_typed_receiver_transitive_looping_method_reports_e0810() {
    assert_sync_preemption_capability(
        r#"
class Work {
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
    pub fn wrapper(self, n: i64) -> i64 {
        return self.heavy(n);
    }
}

async fn run(w: Work) -> i64 {
    return w.wrapper(10);
}
"#,
        &[
            "error[E0810]",
            "sync helper `Work::wrapper` with a loop is not preemptible in task context",
        ],
    );
}

#[test]
fn test_typed_receiver_looping_method_via_local_reports_e0810() {
    assert_sync_preemption_capability(
        r#"
class Work {
    pub init(self) {}
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}

async fn run() -> i64 {
    let w = new Work();
    return w.heavy(10);
}
"#,
        &[
            "error[E0810]",
            "sync helper `Work::heavy` with a loop is not preemptible in task context",
        ],
    );
}

#[test]
fn test_typed_receiver_loop_free_method_is_allowed() {
    let (out, ok) = compile_and_run(
        r#"
class Work {
    pub fn light(self, n: i64) -> i64 {
        return n + 1;
    }
}

async fn run(w: Work) -> i64 {
    return w.light(41);
}

async fn main() {
    println(await run(new Work()));
}
"#,
    );
    assert!(ok, "loop-free typed-receiver method should compile and run");
    assert_eq!(out, "42\n");
}

#[test]
fn test_typed_receiver_looping_method_in_sync_context_is_allowed() {
    // Preemption only matters in a task context; the same call from a plain fn
    // must not warn.
    let (out, ok) = compile_and_run(
        r#"
class Work {
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}

fn run(w: Work) -> i64 {
    return w.heavy(3);
}

fn main() {
    println(run(new Work()));
}
"#,
    );
    assert!(
        ok,
        "looping typed-receiver call in sync context should be allowed"
    );
    assert_eq!(out, "3\n");
}

#[cfg(not(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
)))]
#[test]
fn test_self_looping_method_reports_single_e0810() {
    // `self.heavy()` is handled by the AST-level ConcurrencyAnalyzer; the
    // type-checker typed-receiver path must skip `self` so it is not reported
    // twice.
    let stderr = compile_error_stderr(
        r#"
class Work {
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
    pub async fn run(self) -> i64 {
        return self.heavy(10);
    }
}
"#,
    );
    let count = stderr.matches("error[E0810]").count();
    assert_eq!(
        count, 1,
        "expected exactly one E0810, got {count}:\n{stderr}"
    );
}

#[test]
fn test_typed_receiver_inherited_looping_method_reports_e0810() {
    // The looping method is INHERITED from `Base`; calling it through a
    // `Derived` receiver must still be flagged, attributed to `Base::heavy`.
    assert_sync_preemption_capability(
        r#"
open class Base {
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}

class Derived extends Base {
}

async fn run(d: Derived) -> i64 {
    return d.heavy(10);
}
"#,
        &[
            "error[E0810]",
            "sync helper `Base::heavy` with a loop is not preemptible in task context",
        ],
    );
}

#[test]
fn test_typed_receiver_loop_free_override_is_allowed() {
    // `Derived` overrides the base's looping method with a loop-free body; the
    // call resolves to the override, so it must NOT inherit the base's E0810.
    let (out, ok) = compile_and_run(
        r#"
open class Base {
    pub open fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}

class Derived extends Base {
    pub override fn heavy(self, n: i64) -> i64 {
        return n + 1;
    }
}

async fn run(d: Derived) -> i64 {
    return d.heavy(41);
}

async fn main() {
    println(await run(new Derived()));
}
"#,
    );
    assert!(ok, "loop-free override should not inherit the base's E0810");
    assert_eq!(out, "42\n");
}

// Cross-module typed receiver: a looping method of an IMPORTED class, called
// through a typed receiver in a task context, is flagged with a module note
// (willow-0a6k.2). The receiver-class key differs by import style.

#[cfg(not(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
)))]
#[test]
fn test_cross_module_typed_receiver_item_import_reports_e0810() {
    let m = r#"
pub class Work {
    pub init(self) {}
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}
"#;
    let main = r#"
import m::Work;

async fn run(w: Work) -> i64 {
    return w.heavy(10);
}

fn main() {
    println(1);
}
"#;
    let stderr = compile_temp_project_error_stderr(&[("m.wi", m), ("main.wi", main)], "main.wi");
    for expected in [
        "error[E0810]",
        "sync helper `Work::heavy` with a loop is not preemptible in task context",
        "imported module `m`",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr did not contain `{expected}`:\n{stderr}"
        );
    }
}

#[cfg(not(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
)))]
#[test]
fn test_cross_module_typed_receiver_whole_module_import_reports_e0810() {
    let m = r#"
pub class Work {
    pub init(self) {}
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}
"#;
    let main = r#"
import m;

async fn run(w: m::Work) -> i64 {
    return w.heavy(10);
}

fn main() {
    println(1);
}
"#;
    let stderr = compile_temp_project_error_stderr(&[("m.wi", m), ("main.wi", main)], "main.wi");
    assert!(
        stderr.contains("error[E0810]")
            && stderr.contains("sync helper `m::Work::heavy`")
            && stderr.contains("imported module `m`"),
        "expected whole-module cross-module typed-receiver E0810:\n{stderr}"
    );
}

#[test]
fn test_cross_module_typed_receiver_loop_free_is_allowed() {
    let m = r#"
pub class Work {
    pub init(self) {}
    pub fn light(self, n: i64) -> i64 {
        return n + 1;
    }
}
"#;
    let main = r#"
import m::Work;

async fn run(w: Work) -> i64 {
    return w.light(41);
}

async fn main() {
    println(await run(new Work()));
}
"#;
    let (out, ok) = compile_temp_project_and_run(&[("m.wi", m), ("main.wi", main)], "main.wi");
    assert!(ok, "loop-free cross-module typed-receiver call should run");
    assert_eq!(out, "42\n");
}
