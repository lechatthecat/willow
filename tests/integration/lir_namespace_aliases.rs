//! Builtin namespaces reached through an `import` alias, compiled from Lowered
//! IR (willow-nswv).
//!
//! `import std::fs as files;` makes `files::exists(..)` the same call as
//! `fs::exists(..)`. The former AST emitter never saw the alias: a normalization pass
//! rewrites the whole program before codegen, so it dispatches on canonical
//! names only. The walker lowers to HIR from the RAW frontend program and so
//! still sees whatever name the `import` spelled, and its namespace table is
//! keyed on the canonical one — so every function containing an aliased
//! namespace call fell back to the AST emitter. The answer was right, because
//! the AST emitter produced it; what was wrong is that the walker's coverage
//! depended on how an `import` was written.
//!
//! The fix canonicalizes at the point of dispatch, BEFORE the gate that lets a
//! user module of the same name win — the order the normalization pass and the
//! AST dispatch apply between them, so both paths admit the same set of calls.
//!
//! Since willow-0g8j.3 a body outside the walker's subset is a compile error,
//! so a run that prints the right answer is proof the walker produced it.
//!
//! Nothing asserts on a host string. Paths come from `fs::temp_path`, which is
//! process-unique, the listener binds port 0, and the program name and any OS
//! error message are only ever tested for being non-empty — so the file runs in
//! parallel with itself on macOS, Windows and Linux.
//!
//! 24 perspectives:
//!   1 `fs::exists` under an alias        13 a self-alias (`fs as fs`)
//!   2 write/read roundtrip aliased       14 two modules aliased at once
//!   3 `?` on an aliased void Result      15 an aliased call in a loop
//!   4 `fs::temp_path` aliased            16 an aliased call as an argument
//!   5 the bool is a real bool            17 an aliased call as a scrutinee
//!   6 the `_async` entries too           18 an aliased call in a method
//!   7 `env::args_len` aliased            19 an aliased result across an await
//!   8 `env::program_name` aliased        20 `f64::` is not an alias
//!   9 `env::arg` aliased                 21 GC stress on the aliased path
//!  10 `parallel::map` aliased            22 the walker really compiled these
//!  11 `net::bind` aliased                23 a user module still wins
//!  12 aliased and plain in one file      24 the example is fully LIR

use super::support::{
    compile_and_run_with_env, compile_temp_project_with_env_and_run, compile_with_compiler_env,
};

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "alloc")];

/// Run `source` plainly and under GC stress, and require identical output:
/// collecting at every allocation site must not change what an aliased call
/// returns.
fn assert_aliased(source: &str, expected: &str) {
    let (out, ok) = compile_and_run_with_env(source, &PLAIN);
    assert!(ok, "program failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

/// [`assert_aliased`] plus a third run that collects on every allocation, for
/// the programs that keep a heap string live across another namespace call.
fn assert_aliased_under_stress(source: &str, expected: &str) {
    assert_aliased(source, expected);
    let (out, ok) = compile_and_run_with_env(source, &STRESS);
    assert!(ok, "program failed under GC stress: {out}");
    assert_eq!(out, expected, "wrong output under GC stress");
}

/// Compile once with the selection log on and require the walker to have taken
/// each named function. Without this a coverage regression could leave a
/// function unlowered while the program still printed the right answer.
fn assert_walker_compiled(source: &str, functions: &[&str]) {
    let (ok, stderr) = compile_with_compiler_env(source, &[("WILLOW_LIR_LOG", "1")]);
    assert!(ok, "logged LIR compile failed: {stderr}");
    for function in functions {
        let sync = format!("[lir] compiling `{function}` from lowered IR");
        let coop = format!("[lir] compiling async `{function}` from lowered IR");
        assert!(
            stderr.contains(&sync) || stderr.contains(&coop),
            "`{function}` did not use the LIR walker: {stderr}"
        );
    }
}

/// 1. The call that named the bug. `files::exists` under `import std::fs as
///    files;` is `fs::exists`, and the walker has to emit it as one.
#[test]
fn alias_1_fs_exists_through_an_alias() {
    assert_aliased(
        r#"
import std::fs as files;

fn missing(path: String) -> bool {
    return files::exists(path);
}

fn main() {
    println(missing("willow-no-such-file-alias-1"));
}
"#,
        "false\n",
    );
}

/// 2. A write and a read back, both spelled through the alias, so the payload
///    the second call returns is the one the first stored.
#[test]
fn alias_2_write_and_read_back_through_an_alias() {
    assert_aliased(
        r#"
import std::fs as files;

fn roundtrip(path: String, text: String) -> Result<String, IoError> {
    files::write_string(path, text)?;
    let back = files::read_to_string(path)?;
    return Ok(back);
}

fn main() {
    let path = files::temp_path("willow_alias_2");
    match roundtrip(path, "through the alias") {
        Ok(text) => println(text),
        Err(error) => println("failed"),
    }
    files::remove_file(path);
}
"#,
        "through the alias\n",
    );
}

/// 3. `files::remove_file` returns `Result<void, IoError>`, whose success arm
///    has no payload word — the `?` shape that has to survive the rewrite.
#[test]
fn alias_3_question_mark_on_an_aliased_void_result() {
    assert_aliased(
        r#"
import std::fs as files;

fn store_then_drop(path: String) -> Result<bool, IoError> {
    files::write_string(path, "gone soon")?;
    files::remove_file(path)?;
    return Ok(files::exists(path));
}

fn main() {
    let path = files::temp_path("willow_alias_3");
    match store_then_drop(path) {
        Ok(still_there) => println(still_there),
        Err(error) => println("failed"),
    }
}
"#,
        "false\n",
    );
}

/// 4. `temp_path` takes no path of its own, so it is the entry whose arguments
///    cannot hide a rewrite that only happened to work on a String argument.
#[test]
fn alias_4_temp_path_through_an_alias() {
    assert_aliased(
        r#"
import std::fs as files;

fn somewhere() -> bool {
    let path = files::temp_path("willow_alias_4");
    return path != "";
}

fn main() {
    println(somewhere());
}
"#,
        "true\n",
    );
}

/// 5. `fs::exists` is the one entry the emitter narrows to an `i8`, so an
///    aliased call has to be narrowed too — a raw word would be truthy.
#[test]
fn alias_5_the_aliased_bool_is_a_real_bool() {
    assert_aliased(
        r#"
import std::fs as files;

fn describe(path: String) -> String {
    if files::exists(path) {
        return "present";
    }
    return "absent";
}

fn main() {
    println(describe("willow-no-such-file-alias-5"));
}
"#,
        "absent\n",
    );
}

/// 6. The `_async` entries carry the alias too, and each one parks the task —
///    so the rewrite has to hold on the cooperative path as well.
#[test]
fn alias_6_async_entries_through_an_alias() {
    assert_aliased(
        r#"
import std::fs as files;

async fn roundtrip(path: String, text: String) -> Result<String, IoError> {
    (await files::write_string_async(path, text))?;
    let back = (await files::read_to_string_async(path))?;
    (await files::remove_file_async(path))?;
    return Ok(back);
}

async fn main() {
    let path = files::temp_path("willow_alias_6");
    match await roundtrip(path, "written asynchronously") {
        Ok(text) => println(text),
        Err(error) => println("failed"),
    }
    println(await files::exists_async(path));
}
"#,
        "written asynchronously\nfalse\n",
    );
}

/// 7. `env` is a second namespace, so the map is not special-cased to `fs`.
#[test]
fn alias_7_env_args_len_through_an_alias() {
    assert_aliased(
        r#"
import std::env as sys;

fn count() -> i64 {
    return sys::args_len();
}

fn main() {
    println(count().toString());
}
"#,
        "0\n",
    );
}

/// 8. A String-returning `env` entry, tested for being non-empty rather than
///    for the host's own path.
#[test]
fn alias_8_env_program_name_through_an_alias() {
    assert_aliased(
        r#"
import std::env as sys;

fn named() -> bool {
    return sys::program_name() != "";
}

fn main() {
    println(named());
}
"#,
        "true\n",
    );
}

/// 9. `env::arg` returns `Option<String>`, so the aliased call's result is
///    matched rather than printed.
#[test]
fn alias_9_env_arg_through_an_alias() {
    assert_aliased(
        r#"
import std::env as sys;

fn beyond_the_end() -> String {
    return match sys::arg(99) {
        Some(value) => value,
        None => "none",
    };
}

fn main() {
    println(beyond_the_end());
}
"#,
        "none\n",
    );
}

/// 10. `parallel::map` is the only entry whose arguments are a heap handle and
///     a bare function address, and it is the call the bug was reported on.
#[test]
fn alias_10_parallel_map_through_an_alias() {
    assert_aliased(
        r#"
import std::collections::Array;
import std::parallel as par;

fn square(value: i64) -> i64 {
    return value * value;
}

async fn squares(values: FrozenArray<i64>) -> String {
    let mapped = await par::map(values, square);
    return mapped.toString();
}

async fn main() {
    let values: Array<i64> = [4, 2, 3];
    println(await squares(values.freeze()));
}
"#,
        "[16, 4, 9]\n",
    );
}

/// 11. `net` is the fourth aliasable namespace. Port 0 keeps the address out of
///     the assertion.
#[test]
fn alias_11_net_bind_through_an_alias() {
    assert_aliased(
        r#"
import std::net as sock;

fn bound() -> Result<bool, IoError> {
    let listener = sock::bind("127.0.0.1:0")?;
    let address = sock::local_addr(listener)?;
    return Ok(address != "");
}

fn main() {
    match bound() {
        Ok(reachable) => println(reachable),
        Err(error) => println("failed"),
    }
}
"#,
        "true\n",
    );
}

/// 12. One file, both spellings: an aliased namespace and a plain one dispatch
///     side by side, so the rewrite cannot be a blanket one.
#[test]
fn alias_12_aliased_and_plain_namespaces_in_one_file() {
    assert_aliased(
        r#"
import std::fs as files;
import std::env;

fn both(path: String) -> String {
    if files::exists(path) {
        return "present";
    }
    if env::args_len() == 0 {
        return "absent, no arguments";
    }
    return "absent";
}

fn main() {
    println(both("willow-no-such-file-alias-12"));
}
"#,
        "absent, no arguments\n",
    );
}

/// 13. `import std::fs as fs;` records `fs -> fs`. Resolving it must be
///     idempotent rather than looping or missing.
#[test]
fn alias_13_a_self_alias_resolves_to_itself() {
    assert_aliased(
        r#"
import std::fs as fs;

fn missing(path: String) -> bool {
    return fs::exists(path);
}

fn main() {
    println(missing("willow-no-such-file-alias-13"));
}
"#,
        "false\n",
    );
}

/// 14. Two aliases in one file, so the map is keyed per alias and not collapsed
///     to whichever `import` came last.
#[test]
fn alias_14_two_namespaces_aliased_at_once() {
    assert_aliased(
        r#"
import std::fs as files;
import std::env as sys;

fn report(path: String) -> String {
    if files::exists(path) {
        return "present";
    }
    return "absent " + sys::args_len().toString();
}

fn main() {
    println(report("willow-no-such-file-alias-14"));
}
"#,
        "absent 0\n",
    );
}

/// 15. The aliased call inside a loop body, which is a block of its own — the
///     rewrite is per call site, not per function.
#[test]
fn alias_15_an_aliased_call_in_a_loop() {
    assert_aliased(
        r#"
import std::fs as files;

fn none_of(path: String, rounds: i64) -> i64 {
    let mut found = 0;
    let mut i = 0;
    while i < rounds {
        if files::exists(path) {
            found = found + 1;
        }
        i = i + 1;
    }
    return found;
}

fn main() {
    println(none_of("willow-no-such-file-alias-15", 3).toString());
}
"#,
        "0\n",
    );
}

/// 16. The aliased call in argument position, so its value is produced into
///     someone else's parameter slot rather than straight into a `let`.
#[test]
fn alias_16_an_aliased_call_as_an_argument() {
    assert_aliased(
        r#"
import std::fs as files;

fn invert(value: bool) -> bool {
    return !value;
}

fn main() {
    println(invert(files::exists("willow-no-such-file-alias-16")));
}
"#,
        "true\n",
    );
}

/// 17. The aliased call as a `match` scrutinee, which the walker vets as an
///     operand of the match rather than as a statement.
#[test]
fn alias_17_an_aliased_call_as_a_match_scrutinee() {
    assert_aliased(
        r#"
import std::fs as files;

fn describe(path: String) -> String {
    return match files::read_to_string(path) {
        Ok(text) => text,
        Err(error) => "unreadable",
    };
}

fn main() {
    println(describe("willow-no-such-file-alias-17"));
}
"#,
        "unreadable\n",
    );
}

/// 18. Inside a class method, which the backend compiles under a mangled name
///     of its own — the alias map is per FILE, not per function.
#[test]
fn alias_18_an_aliased_call_in_a_method() {
    assert_aliased(
        r#"
import std::fs as files;

class Store {
    pub path: String;

    pub fn present(self) -> bool {
        return files::exists(self.path);
    }
}

fn main() {
    let store = new Store("willow-no-such-file-alias-18");
    println(store.present());
}
"#,
        "false\n",
    );
}

/// 19. The String an aliased read produced, still live across a suspension and
///     across a second aliased call.
#[test]
fn alias_19_an_aliased_result_across_an_await() {
    assert_aliased(
        r#"
import std::fs as files;

async fn keep(path: String, text: String) -> String {
    (await files::write_string_async(path, text));
    let back = match await files::read_to_string_async(path) {
        Ok(loaded) => loaded,
        Err(error) => "unreadable",
    };
    (await files::remove_file_async(path));
    let gone = await files::exists_async(path);
    if gone {
        return "still there";
    }
    return back;
}

async fn main() {
    let path = files::temp_path("willow_alias_19");
    println(await keep(path, "survived the park"));
}
"#,
        "survived the park\n",
    );
}

/// 20. `f64::to_string` and `f64::parse` ride the same dispatch table but are
///     answered before the alias map, so a file full of aliases cannot move
///     them.
#[test]
fn alias_20_f64_calls_are_untouched_by_an_alias_map() {
    assert_aliased(
        r#"
import std::fs as files;
import std::env as sys;

fn rendered() -> bool {
    return f64::to_string(1.5) != "";
}

fn parsed() -> bool {
    return match f64::parse("2.5") {
        Ok(value) => value > 2.0,
        Err(error) => false,
    };
}

fn main() {
    println(rendered());
    println(parsed());
    println(files::exists("willow-no-such-file-alias-20"));
    println(sys::args_len().toString());
}
"#,
        "true\ntrue\nfalse\n0\n",
    );
}

/// 21. A heap String from an aliased read held live across two more aliased
///     calls, collected on every allocation.
#[test]
fn alias_21_gc_stress_on_the_aliased_path() {
    assert_aliased_under_stress(
        r#"
import std::fs as files;

fn hold(path: String, text: String) -> Result<String, IoError> {
    files::write_string(path, text)?;
    let back = files::read_to_string(path)?;
    files::remove_file(path)?;
    let again = files::exists(path);
    if again {
        return Ok("still there");
    }
    return Ok(back + "!");
}

fn main() {
    let path = files::temp_path("willow_alias_21");
    match hold(path, "held across a collection") {
        Ok(text) => println(text),
        Err(error) => println("failed"),
    }
}
"#,
        "held across a collection!\n",
    );
}

/// 22. The selection log explicitly records each lowered aliased call.
#[test]
fn alias_22_the_walker_really_compiled_the_aliased_calls() {
    assert_walker_compiled(
        r#"
import std::collections::Array;
import std::fs as files;
import std::env as sys;
import std::parallel as par;
import std::net as sock;

fn square(value: i64) -> i64 {
    return value * value;
}

fn missing(path: String) -> bool {
    return files::exists(path);
}

fn count() -> i64 {
    return sys::args_len();
}

fn bound() -> Result<bool, IoError> {
    let listener = sock::bind("127.0.0.1:0")?;
    let address = sock::local_addr(listener)?;
    return Ok(address != "");
}

async fn squares(values: FrozenArray<i64>) -> String {
    let mapped = await par::map(values, square);
    return mapped.toString();
}

async fn main() {
    println(missing("willow-no-such-file-alias-22"));
    println(count().toString());
    let values: Array<i64> = [1];
    println(await squares(values.freeze()));
    match bound() {
        Ok(reachable) => println(reachable),
        Err(error) => println("failed"),
    }
}
"#,
        &["square", "missing", "count", "bound", "squares", "main"],
    );
}

/// 23. The invariant the alias resolution must not break: a USER module
///     registered under the canonical name still wins over the builtin, and an
///     alias of the std module reaches the std one from the same file.
///
///     What the build proves is only that nothing here is outside the subset;
///     which module each call reached is what the printed values say.
#[test]
fn alias_23_a_user_module_of_the_same_name_still_wins() {
    let files = [
        (
            "src/fs.wi",
            r#"
pub fn exists(path: String) -> bool {
    return path == "mine";
}
"#,
        ),
        (
            "src/main.wi",
            r#"
import std::fs as files;
import fs;

fn main() {
    println(fs::exists("mine"));
    println(fs::exists("willow-no-such-file-alias-23"));
    println(files::exists("willow-no-such-file-alias-23"));
}
"#,
        ),
    ];
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "src/main.wi", &PLAIN);
    assert!(ok, "build failed: {out}");
    // Line 1 is the user module answering `true`; lines 2 and 3 are the user
    // module and the std module answering `false` about a file that is not
    // there — so `fs::` reached the user's and `files::` reached std's.
    assert_eq!(out, "true\nfalse\nfalse\n");
}

/// 24. The shipped example, which mixes all four namespaces and both spellings,
///     compiles end to end with every function claimed by the walker.
#[test]
fn alias_24_the_example_is_fully_lir() {
    let source = std::fs::read_to_string("example/lir_namespace_aliases.wi")
        .expect("example/lir_namespace_aliases.wi");
    assert_walker_compiled(
        &source,
        &[
            "square",
            "roundtrip",
            "is_failure",
            "async_roundtrip",
            "arg_count",
            "squares",
            "bound",
            "main",
        ],
    );
}
