// Release synchronous code publishes no fault site (willow-9686).
//
// Release synchronous runtime faults are location-free (willow-s9ej.7), so
// generated synchronous code must not call `willow_trace_site_set` before every
// raising operation: the call cost a hot map loop ~2x and spilled loop-invariant
// registers in array loops. Async code still publishes its own site, and a site
// it published must not label a fault in synchronous code it calls, directly,
// through a function value, method or interface, after an await, or in a runtime task.

use super::support::*;
use std::process::Command;

const TRACE_SITE: &str = "willow_trace_site_set";

fn trace_site_calls(source: &str, release: bool) -> usize {
    compile_and_collect_relocations_by_section(source, &[], release)
        .into_iter()
        .filter(|(section, name)| section == "text" && name == TRACE_SITE)
        .count()
}

/// Build in the requested mode and return `(stdout+stderr, exit ok)`.
fn run_mode(source: &str, release: bool) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_fault_site_{id}.wi"));
    let bin_path = temp_path(format!("willow_fault_site_{id}"));
    std::fs::write(&src_path, source).unwrap();
    let mut args = vec!["build", &src_path, "-o", &bin_path];
    if release {
        args.push("--release");
    }
    let build = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(&args)
        .output()
        .expect("failed to run compiler");
    assert!(
        build.status.success(),
        "compile failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");
    let _ = std::fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
    (
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        out.status.success(),
    )
}

/// The `runtime panic:` header line of an unhandled fault.
fn panic_header(source: &str, release: bool, message: &str) -> (String, String) {
    let (out, ok) = run_mode(source, release);
    assert!(!ok, "expected a runtime fault: {out}");
    let header = out
        .lines()
        .find(|line| line.starts_with("runtime panic:"))
        .unwrap_or_else(|| panic!("no panic header: {out}"))
        .to_string();
    assert!(header.contains(message), "unexpected fault: {out}");
    (header, out)
}

fn assert_location_free(source: &str, message: &str) -> String {
    let (header, out) = panic_header(source, true, message);
    assert!(
        !header.contains(" at "),
        "release synchronous fault inherited a location: {out}"
    );
    out
}

const SYNC_ARRAY_LOOP: &str = r#"
import std::collections::Array;
fn main() {
    let values: Array<i64> = [];
    let mut i = 0;
    while i < 64 { values.push(i); i = i + 1; }
    let mut sum = 0;
    i = 0;
    while i < 64 { values[i] = values[i] + 1; sum = sum + values[i]; i = i + 1; }
    println(sum);
}
"#;

const SYNC_MAP_LOOP: &str = r#"
import std::collections::Map;
fn main() {
    let mut table: Map<i64, i64> = Map::new();
    let mut i: i64 = 0;
    while i < 1000 { table.insert(i * 2654435761, i); i = i + 1; }
    let mut sum: i64 = 0;
    i = 0;
    while i < 2000 {
        let found = match table.get(i * 2654435761) {
            Option::Some(value) => value,
            Option::None => 1,
        };
        sum = sum + found;
        i = i + 1;
    }
    println(sum);
}
"#;

const SYNC_CALLS: &str = r#"
import std::collections::Array;
interface Reader { fn read(self, i: i64) -> i64; }
class Box implements Reader {
    pub values: Array<i64>;
    pub fn read(self, i: i64) -> i64 { return self.values[i]; }
}
fn adder(n: i64) -> closure(i64) -> i64 { return |x| x + n; }
fn get(values: Array<i64>, i: i64) -> i64 { return values[i]; }
fn main() {
    let values: Array<i64> = [3, 4];
    let b = new Box(values);
    let r: Reader = b;
    let f = adder(2);
    println(get(values, 1) + b.read(0) + r.read(1) + f(5));
}
"#;

#[test]
fn rfs_01_release_sync_array_loop_publishes_no_site() {
    assert_eq!(trace_site_calls(SYNC_ARRAY_LOOP, true), 0);
    let (out, ok) = run_mode(SYNC_ARRAY_LOOP, true);
    assert!(ok, "{out}");
    assert_eq!(out, "2080\n");
}

#[test]
fn rfs_02_release_sync_map_loop_publishes_no_site() {
    assert_eq!(trace_site_calls(SYNC_MAP_LOOP, true), 0);
    let (out, ok) = run_mode(SYNC_MAP_LOOP, true);
    assert!(ok, "{out}");
    assert_eq!(out, "500500\n");
}

#[test]
fn rfs_03_release_sync_calls_publish_no_site() {
    assert_eq!(trace_site_calls(SYNC_CALLS, true), 0);
    let (out, ok) = run_mode(SYNC_CALLS, true);
    assert!(ok, "{out}");
    assert_eq!(out, "18\n");
}

#[test]
fn rfs_04_release_sync_site_count_stays_zero_as_raising_operations_grow() {
    for count in [1usize, 16, 64] {
        let body: String = (0..count)
            .map(|i| format!("    sum = sum + values[{}];\n", i % 2))
            .collect();
        let source = format!(
            "import std::collections::Array;\nfn main() {{\n    let values: Array<i64> = [1, 2];\n    let mut sum = 0;\n{body}    println(sum);\n}}\n"
        );
        assert_eq!(trace_site_calls(&source, true), 0, "count {count}");
    }
}

#[test]
fn rfs_05_debug_sync_code_still_publishes_its_leaf() {
    assert!(trace_site_calls(SYNC_ARRAY_LOOP, false) > 0);
    assert!(trace_site_calls(SYNC_MAP_LOOP, false) > 0);
}

#[test]
fn rfs_06_release_async_code_still_publishes_its_site() {
    let source = r#"
import std::collections::Array;
async fn work(i: i64) -> i64 {
    let values: Array<i64> = [1, 2];
    return values[i];
}
async fn main() { println(await work(1)); }
"#;
    assert!(trace_site_calls(source, true) > 0);
}

#[test]
fn rfs_07_release_sync_fault_is_location_free() {
    assert_location_free(
        r#"
import std::collections::Array;
fn main() {
    let values: Array<i64> = [1, 2];
    let i = 7;
    println(values[i]);
}
"#,
        "index out of bounds",
    );
}

#[test]
fn rfs_08_release_async_direct_fault_keeps_its_location() {
    let (header, out) = panic_header(
        r#"
import std::collections::Array;
async fn work() -> i64 {
    let values: Array<i64> = [1, 2];
    return values[5];
}
async fn main() { println(await work()); }
"#,
        true,
        "index out of bounds",
    );
    assert!(header.ends_with(":5:18"), "{out}");
    assert!(out.contains("async work at "), "{out}");
}

#[test]
fn rfs_09_release_async_caller_site_does_not_label_a_sync_callee_fault() {
    let out = assert_location_free(
        r#"
fn boom(ch: Channel<i64>) -> i64 { return ch.recv(); }
async fn work() -> i64 {
    let ch = Channel<i64>::new();
    ch.close();
    return boom(ch);
}
async fn main() { println(await work()); }
"#,
        "recv on closed empty channel",
    );
    assert!(out.contains("async work at "), "{out}");
}

#[test]
fn rfs_10_release_async_function_value_call_fault_is_location_free() {
    assert_location_free(
        r#"
import std::collections::Array;
fn get(values: Array<i64>, i: i64) -> i64 { return values[i]; }
async fn work() -> i64 {
    let values: Array<i64> = [1, 2];
    let f: fn(Array<i64>, i64) -> i64 = get;
    return f(values, 9);
}
async fn main() { println(await work()); }
"#,
        "index out of bounds",
    );
}

#[test]
fn rfs_11_release_async_method_call_fault_is_location_free() {
    assert_location_free(
        r#"
import std::collections::Array;
class Box {
    pub values: Array<i64>;
    pub fn read(self, i: i64) -> i64 { return self.values[i]; }
}
async fn work() -> i64 {
    let b = new Box([1, 2]);
    return b.read(4);
}
async fn main() { println(await work()); }
"#,
        "index out of bounds",
    );
}

#[test]
fn rfs_12_release_async_interface_call_fault_is_location_free() {
    assert_location_free(
        r#"
import std::collections::Array;
interface Reader { fn read(self, i: i64) -> i64; }
class Box implements Reader {
    pub values: Array<i64>;
    pub fn read(self, i: i64) -> i64 { return self.values[i]; }
}
fn make() -> Reader { return new Box([1, 2]); }
async fn work() -> i64 {
    return make().read(4);
}
async fn main() { println(await work()); }
"#,
        "index out of bounds",
    );
}

#[test]
fn rfs_13_release_sync_fault_after_an_await_is_location_free() {
    assert_location_free(
        r#"
import std::collections::Array;
fn boom(values: Array<i64>) -> i64 { return values[3]; }
async fn work() -> i64 {
    let values: Array<i64> = [1, 2];
    return values[1];
}
async fn main() {
    println(await work());
    let values: Array<i64> = [1, 2];
    println(boom(values));
}
"#,
        "index out of bounds",
    );
}

#[test]
fn rfs_14_release_nested_sync_callees_stay_location_free() {
    assert_location_free(
        r#"
import std::collections::Array;
fn inner(values: Array<i64>, i: i64) -> i64 { return values[i]; }
fn outer(values: Array<i64>) -> i64 { return inner(values, 1) + inner(values, 8); }
async fn work() -> i64 {
    let values: Array<i64> = [1, 2];
    return outer(values);
}
async fn main() { println(await work()); }
"#,
        "index out of bounds",
    );
}

#[test]
fn rfs_15_release_parallel_mapper_fault_is_location_free() {
    assert_location_free(
        r#"
import std::collections::Array;
import std::parallel;
fn bad(n: i64) -> i64 {
    let values: Array<i64> = [1, 2];
    return values[n];
}
async fn main() {
    let xs: Array<i64> = [0, 1, 7];
    let out = await parallel::map(xs.freeze(), bad);
    println(out[0]);
}
"#,
        "index out of bounds",
    );
}

#[test]
fn rfs_16_release_async_fault_after_a_sync_call_republishes_its_site() {
    let (header, out) = panic_header(
        r#"
import std::collections::Array;
fn helper(values: Array<i64>) -> i64 { return values[0]; }
async fn work() -> i64 {
    let values: Array<i64> = [1, 2];
    let first = helper(values);
    return first + values[6];
}
async fn main() { println(await work()); }
"#,
        true,
        "index out of bounds",
    );
    assert!(header.ends_with(":7:26"), "{out}");
}

#[test]
fn rfs_17_debug_sync_callee_of_async_keeps_its_own_location() {
    let (header, out) = panic_header(
        r#"
import std::collections::Array;
fn boom(values: Array<i64>) -> i64 {
    return values[3];
}
async fn work() -> i64 {
    let values: Array<i64> = [1, 2];
    return boom(values);
}
async fn main() { println(await work()); }
"#,
        false,
        "index out of bounds",
    );
    assert!(header.ends_with(":4:18"), "{out}");
    assert!(out.contains("0: boom at "), "{out}");
}

#[test]
fn rfs_18_debug_sync_fault_keeps_its_location() {
    let (header, out) = panic_header(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    ch.close();
    println(ch.recv());
}
"#,
        false,
        "recv on closed empty channel",
    );
    assert!(header.ends_with(":5:16"), "{out}");
}

#[test]
fn rfs_19_release_and_debug_sync_programs_agree() {
    for source in [SYNC_ARRAY_LOOP, SYNC_MAP_LOOP, SYNC_CALLS] {
        assert_eq!(run_mode(source, true), run_mode(source, false));
    }
}

#[test]
fn rfs_20_release_sync_main_after_spawned_tasks_is_location_free() {
    assert_location_free(
        r#"
import std::collections::Array;
async fn work(i: i64) -> i64 {
    let values: Array<i64> = [1, 2, 3];
    return values[i];
}
fn boom() -> i64 {
    let ch = Channel<i64>::new();
    ch.close();
    return ch.recv();
}
async fn main() {
    let a = work(0);
    let b = work(2);
    println(await a + await b);
    println(boom());
}
"#,
        "recv on closed empty channel",
    );
}
