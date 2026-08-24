//! `std::fs`, `std::net` and the prelude's error enums compiled from Lowered IR
//! (willow-0g8j.2.13).
//!
//! Four pieces land together here, because a real I/O function needs all four
//! at once:
//!
//! * the `fs::` and `net::` namespaces, dispatched from one table whose
//!   signatures are read from the stdlib schema rather than transcribed;
//! * `TcpListener` and `TcpStream` as types — opaque runtime words, admitted
//!   after the class path so a program of its own declares them wins;
//! * `?` on a `Result<void, E>`, which every write in both namespaces returns
//!   and whose success arm has no payload word to read;
//! * `f64::to_string` and `f64::parse`, the two fixed-signature runtime calls
//!   that are spelled with a `::` but reach no module — they ride the same
//!   dispatch table;
//! * the prelude's error enums in `match` patterns. `Enums::with_prelude` used
//!   to transcribe `Option` and `Result` alone, so `Failed(message)` on an
//!   `IoError` missed the variant lookup in HIR lowering and lowered as an
//!   interface downcast onto a class named `Failed` — a type that does not
//!   exist. It is read from the prelude source now.
//!
//! Every test asserts the same output from the AST emitter and the walker, and
//! confirms the walker is the path that ran, so a coverage regression that sent
//! a function back to the AST emitter fails here rather than passing vacuously
//! by comparing the AST path against itself.
//!
//! Nothing asserts on an OS error string: the message in an `IoError` is the
//! host's, so the tests check that one is CARRIED, never what it says. Paths
//! come from `fs::temp_path` (process-unique) and the listener binds port 0, so
//! the file runs in parallel with itself on macOS, Windows and Linux.
//!
//! 24 perspectives:
//!   1 write/read/remove roundtrip     13 `env::` still dispatches
//!   2 `?` on a `Result<void, E>`      14 bind + local_addr
//!   3 `fs::exists` is a real bool     15 a loopback write/read exchange
//!   4 a missing read takes `Err`      16 shutdown, two void `?` in a row
//!   5 the error payload is carried    17 a stream in and out of a function
//!   6 an `IoError` crosses a call     18 a listener in a class field
//!   7 `ParseFloatError` matches too   19 handles in an array
//!   8 a program enum shadows it       20 two tasks, two exchanges
//!   9 async write/read/remove         21 GC stress with live handles
//!  10 `exists_async` across a park    22 a handle survives a suspension
//!  11 a void `?` inside a loop        23 a void `?` before a suspension
//!  12 nested match on the error       24 the example is fully LIR

use super::support::{compile_and_run_with_env, compile_with_compiler_env};

const AST: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];
const LIR: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")];
const LIR_BUDGET: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_TASK_BUDGET", "1"),
];
const LIR_STRESS: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_GC_STRESS", "alloc"),
];

/// Run `source` under the AST emitter and under the walker (plain, with the
/// one-step task budget, and under allocation stress), asserting the same
/// output every time, then assert that each of `functions` really was compiled
/// from lowered IR.
fn assert_io(source: &str, expected: &str, functions: &[&str]) {
    for env in [&AST[..], &LIR[..], &LIR_BUDGET[..], &LIR_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "io run failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
    let (ok, stderr) = compile_with_compiler_env(
        source,
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_LIR_LOG", "1"),
        ],
    );
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

// 1. the base case: a file is written, read back, and removed, with every step
//    threaded through `?`.
#[test]
fn lir_io_01_write_read_remove_roundtrip() {
    let source = r#"
import std::fs;

fn roundtrip(path: String, text: String) -> Result<String, IoError> {
    fs::write_string(path, text)?;
    let back = fs::read_to_string(path)?;
    fs::remove_file(path)?;
    return Ok(back);
}

fn main() {
    let path = fs::temp_path("willow_lir_io_01");
    match roundtrip(path, "round trip") {
        Ok(text) => println(text),
        Err(error) => println("io failed"),
    }
}
"#;
    assert_io(source, "round trip\n", &["roundtrip", "main"]);
}

// 2. `fs::write_string` returns `Result<void, IoError>`, so its `?` produces no
//    value at all: the `Ok` object is the tag word alone and the success arm
//    must read nothing rather than load a word 1 that was never written.
#[test]
fn lir_io_02_void_result_propagation_reads_no_payload() {
    let source = r#"
import std::fs;

fn write_twice(path: String) -> Result<String, IoError> {
    fs::write_string(path, "first")?;
    fs::write_string(path, "second")?;
    let back = fs::read_to_string(path)?;
    fs::remove_file(path)?;
    return Ok(back);
}

fn main() {
    let path = fs::temp_path("willow_lir_io_02");
    match write_twice(path) {
        Ok(text) => println(text),
        Err(error) => println("io failed"),
    }
}
"#;
    assert_io(source, "second\n", &["write_twice", "main"]);
}

// 3. `fs::exists` is the one namespace entry whose runtime hands back a full
//    word for a `bool`. Narrowed wrongly it would still print `true`/`false`,
//    so the test straddles a write: false, then true, then false again.
#[test]
fn lir_io_03_exists_is_a_real_bool() {
    let source = r#"
import std::fs;

fn probe(path: String) -> Result<i64, IoError> {
    println(fs::exists(path));
    fs::write_string(path, "here")?;
    println(fs::exists(path));
    fs::remove_file(path)?;
    println(fs::exists(path));
    return Ok(0);
}

fn main() {
    let path = fs::temp_path("willow_lir_io_03");
    match probe(path) {
        Ok(n) => println(n),
        Err(error) => println(-1),
    }
}
"#;
    assert_io(source, "false\ntrue\nfalse\n0\n", &["probe", "main"]);
}

// 4. the failure half of `?`: a read of a path that does not exist leaves the
//    function through the error arm.
#[test]
fn lir_io_04_missing_read_takes_the_error_arm() {
    let source = r#"
import std::fs;

fn read(path: String) -> Result<String, IoError> {
    let text = fs::read_to_string(path)?;
    return Ok(text);
}

fn main() {
    let path = fs::temp_path("willow_lir_io_04");
    match read(path) {
        Ok(text) => println("unexpected"),
        Err(error) => println("missing"),
    }
}
"#;
    assert_io(source, "missing\n", &["read", "main"]);
}

// 5. the propagated `IoError` carries its payload: the unqualified
//    `Failed(message)` pattern binds the host's message, which is non-empty.
//    The text itself is the OS's and is never asserted.
#[test]
fn lir_io_05_error_payload_is_carried() {
    let source = r#"
import std::fs;

fn read(path: String) -> Result<String, IoError> {
    let text = fs::read_to_string(path)?;
    return Ok(text);
}

fn main() {
    let path = fs::temp_path("willow_lir_io_05");
    match read(path) {
        Ok(text) => println("unexpected"),
        Err(error) => {
            match error {
                Failed(message) => println(message != ""),
            }
        }
    }
}
"#;
    assert_io(source, "true\n", &["read", "main"]);
}

// 6. an `IoError` is an ordinary value: it crosses a call boundary and is
//    matched there, not at the site that produced it.
#[test]
fn lir_io_06_error_crosses_a_call() {
    let source = r#"
import std::fs;

fn describe(error: IoError) -> bool {
    return match error {
        Failed(message) => message != "",
    };
}

fn read(path: String) -> Result<String, IoError> {
    let text = fs::read_to_string(path)?;
    return Ok(text);
}

fn main() {
    let path = fs::temp_path("willow_lir_io_06");
    match read(path) {
        Ok(text) => println("unexpected"),
        Err(error) => println(describe(error)),
    }
}
"#;
    assert_io(source, "true\n", &["describe", "read", "main"]);
}

// 7. the fix is not about `IoError`: every enum the prelude declares is read
//    from the prelude source, so `ParseFloatError` matches by variant too.
#[test]
fn lir_io_07_parse_float_error_matches_by_variant() {
    let source = r#"
fn code(error: ParseFloatError) -> i64 {
    return match error {
        Invalid(message) => 0,
    };
}

fn parse(text: String) -> i64 {
    return match f64::parse(text) {
        Ok(value) => 1,
        Err(error) => code(error),
    };
}

fn main() {
    println(parse("2.5"));
    println(parse("not a number"));
}
"#;
    assert_io(source, "1\n0\n", &["code", "parse", "main"]);
}

// 8. a program enum of the same name still shadows the prelude's, the order
//    `register_prelude` gives the checker.
#[test]
fn lir_io_08_program_enum_shadows_the_prelude_one() {
    let source = r#"
enum IoError { Broken(i64) }

fn code(error: IoError) -> i64 {
    return match error {
        Broken(n) => n,
    };
}

fn main() {
    println(code(Broken(7)));
}
"#;
    assert_io(source, "7\n", &["code", "main"]);
}

// 9. the async half of the namespace: the same roundtrip driven by tasks, so
//    every call parks and resumes in a poll function.
#[test]
fn lir_io_09_async_write_read_remove() {
    let source = r#"
import std::fs;

async fn roundtrip(path: String, text: String) -> Result<String, IoError> {
    (await fs::write_string_async(path, text))?;
    let back = (await fs::read_to_string_async(path))?;
    (await fs::remove_file_async(path))?;
    return Ok(back);
}

async fn main() {
    let path = fs::temp_path("willow_lir_io_09");
    match await roundtrip(path, "from a task") {
        Ok(text) => println(text),
        Err(error) => println("io failed"),
    }
}
"#;
    assert_io(source, "from a task\n", &["roundtrip", "main"]);
}

// 10. `exists_async` hands a `bool` back across a park — the narrowing again,
//     this time through an async frame's result slot.
#[test]
fn lir_io_10_exists_async_across_a_park() {
    let source = r#"
import std::fs;

async fn probe(path: String) -> Result<i64, IoError> {
    println(await fs::exists_async(path));
    (await fs::write_string_async(path, "here"))?;
    println(await fs::exists_async(path));
    (await fs::remove_file_async(path))?;
    println(await fs::exists_async(path));
    return Ok(0);
}

async fn main() {
    let path = fs::temp_path("willow_lir_io_10");
    match await probe(path) {
        Ok(n) => println(n),
        Err(error) => println(-1),
    }
}
"#;
    assert_io(source, "false\ntrue\nfalse\n0\n", &["probe", "main"]);
}

// 11. a void `?` inside a loop: the early return leaves the loop and the
//     function at once, and the success path keeps going round.
#[test]
fn lir_io_11_void_propagation_inside_a_loop() {
    let source = r#"
import std::fs;

fn write_many(path: String, times: i64) -> Result<String, IoError> {
    let mut i = 0;
    while i < times {
        fs::write_string(path, "line")?;
        i = i + 1;
    }
    let back = fs::read_to_string(path)?;
    fs::remove_file(path)?;
    return Ok(back + i.toString());
}

fn main() {
    let path = fs::temp_path("willow_lir_io_11");
    match write_many(path, 4) {
        Ok(text) => println(text),
        Err(error) => println("io failed"),
    }
}
"#;
    assert_io(source, "line4\n", &["write_many", "main"]);
}

// 12. the shape `example/file_io.wi` is written in: a match on the `Result`
//     whose error arm opens a second match on the `IoError` inside it.
#[test]
fn lir_io_12_nested_match_on_the_error() {
    let source = r#"
import std::fs;

fn save(path: String) -> Result<String, IoError> {
    fs::write_string(path, "saved")?;
    let text = fs::read_to_string(path)?;
    fs::remove_file(path)?;
    return Ok(text);
}

fn main() {
    let path = fs::temp_path("willow_lir_io_12");
    match save(path) {
        Ok(text) => println(text),
        Err(error) => {
            match error {
                Failed(message) => println("failed"),
            }
        }
    }
    match fs::read_to_string(path) {
        Ok(text) => println(text),
        Err(error) => {
            match error {
                Failed(message) => println("failed"),
            }
        }
    }
}
"#;
    assert_io(source, "saved\nfailed\n", &["save", "main"]);
}

// 13. the `env::` namespace shared the rewrite that added `fs::` and `net::`,
//     so it is re-pinned here rather than assumed.
#[test]
fn lir_io_13_env_namespace_still_dispatches() {
    let source = r#"
import std::env;

fn describe() -> i64 {
    println(env::program_name() != "");
    println(env::args_len());
    return 0;
}

fn main() {
    println(describe());
}
"#;
    assert_io(source, "true\n0\n0\n", &["describe", "main"]);
}

// 14. the listener: bound to port 0 so the OS picks the port, and its address
//     read back out. Both are `Result<_, IoError>` and both are `?`-threaded.
#[test]
fn lir_io_14_bind_and_local_addr() {
    let source = r#"
import std::net;

fn bound_address() -> Result<bool, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    return Ok(address != "");
}

fn main() {
    match bound_address() {
        Ok(bound) => println(bound),
        Err(error) => println(false),
    }
}
"#;
    assert_io(source, "true\n", &["bound_address", "main"]);
}

// 15. the whole exchange: accept and connect in flight together, then a write
//     and a read that must deliver the same bytes.
#[test]
fn lir_io_15_loopback_exchange() {
    let source = r#"
import std::net;

async fn exchange(message: String) -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    (await net::write_async(client, message))?;
    let server = (await accepting)?;
    let got = (await net::read_async(server, 4096))?;
    return Ok(got);
}

async fn main() {
    match await exchange("over loopback") {
        Ok(text) => println(text),
        Err(error) => println("net failed"),
    }
}
"#;
    assert_io(source, "over loopback\n", &["exchange", "main"]);
}

// 16. two `Result<void, IoError>` propagations back to back, each on a
//     different handle, with a read between them.
#[test]
fn lir_io_16_shutdown_both_ends() {
    let source = r#"
import std::net;

async fn exchange(message: String) -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    (await net::write_async(client, message))?;
    let server = (await accepting)?;
    let got = (await net::read_async(server, 4096))?;
    net::shutdown(client)?;
    net::shutdown(server)?;
    return Ok(got);
}

async fn main() {
    match await exchange("closed cleanly") {
        Ok(text) => println(text),
        Err(error) => println("net failed"),
    }
}
"#;
    assert_io(source, "closed cleanly\n", &["exchange", "main"]);
}

// 17. a `TcpStream` as a parameter and as a return value: the handle is an
//     ordinary word the walker passes and receives.
#[test]
fn lir_io_17_stream_in_and_out_of_a_function() {
    let source = r#"
import std::net;

async fn greet(stream: TcpStream, message: String) -> Result<TcpStream, IoError> {
    (await net::write_async(stream, message))?;
    return Ok(stream);
}

async fn exchange(message: String) -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    let sent = (await greet(client, message))?;
    let server = (await accepting)?;
    let got = (await net::read_async(server, 4096))?;
    net::shutdown(sent)?;
    return Ok(got);
}

async fn main() {
    match await exchange("passed along") {
        Ok(text) => println(text),
        Err(error) => println("net failed"),
    }
}
"#;
    assert_io(source, "passed along\n", &["greet", "exchange", "main"]);
}

// 18. a `TcpListener` stored in a class field, which is what makes the handle a
//     type question rather than a call question: the field layout has to admit
//     it before any method can read it back.
#[test]
fn lir_io_18_listener_in_a_class_field() {
    let source = r#"
import std::net;

class Endpoint {
    pub listener: TcpListener;
    pub address: String;
}

async fn serve(endpoint: Endpoint, message: String) -> Result<String, IoError> {
    let accepting = net::accept_async(endpoint.listener);
    let client = (await net::connect_async(endpoint.address))?;
    (await net::write_async(client, message))?;
    let server = (await accepting)?;
    let got = (await net::read_async(server, 4096))?;
    return Ok(got);
}

async fn run(message: String) -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let endpoint = new Endpoint(listener, address);
    let got = (await serve(endpoint, message))?;
    return Ok(got);
}

async fn main() {
    let outcome = await run("through a field");
    match outcome {
        Ok(text) => println(text),
        Err(error) => println("net failed"),
    }
}
"#;
    assert_io(source, "through a field\n", &["serve", "run", "main"]);
}

// 19. handles in an array: the element type has to be admitted for the array to
//     be, and the two ends are then indexed rather than named.
#[test]
fn lir_io_19_handles_in_an_array() {
    let source = r#"
import std::net;

async fn exchange(message: String) -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    let server_pending = accepting;
    (await net::write_async(client, message))?;
    let server = (await server_pending)?;
    let streams = [client, server];
    let got = (await net::read_async(streams[1], 4096))?;
    net::shutdown(streams[0])?;
    net::shutdown(streams[1])?;
    return Ok(got);
}

async fn main() {
    match await exchange("indexed") {
        Ok(text) => println(text),
        Err(error) => println("net failed"),
    }
}
"#;
    assert_io(source, "indexed\n", &["exchange", "main"]);
}

// 20. two exchanges in flight at once, each on its own listener, so the two
//     sets of handles must not be confused for one another.
#[test]
fn lir_io_20_two_tasks_two_exchanges() {
    let source = r#"
import std::net;

async fn exchange(message: String) -> String {
    match await attempt(message) {
        Ok(text) => return text,
        Err(error) => return "net failed",
    }
}

async fn attempt(message: String) -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    (await net::write_async(client, message))?;
    let server = (await accepting)?;
    let got = (await net::read_async(server, 4096))?;
    return Ok(got);
}

async fn main() {
    let first = exchange("first");
    let second = exchange("second");
    println(await first);
    println(await second);
}
"#;
    assert_io(source, "first\nsecond\n", &["exchange", "attempt", "main"]);
}

// 21. the handles under allocation stress. `assert_io` already runs every case
//     with WILLOW_GC_STRESS=alloc; this one allocates between the two ends of
//     the exchange, so a collection is guaranteed to land while both are live.
#[test]
fn lir_io_21_handles_under_gc_stress() {
    let source = r#"
import std::net;

async fn exchange(message: String) -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    let mut noise = [0];
    let mut i = 0;
    while i < 64 {
        noise = [i, i + 1, i + 2];
        i = i + 1;
    }
    (await net::write_async(client, message))?;
    let server = (await accepting)?;
    let got = (await net::read_async(server, 4096))?;
    return Ok(got + " " + noise[0].toString());
}

async fn main() {
    match await exchange("survived") {
        Ok(text) => println(text),
        Err(error) => println("net failed"),
    }
}
"#;
    assert_io(source, "survived 63\n", &["exchange", "main"]);
}

// 22. a handle held across a suspension that is not its own: the listener is
//     bound, the task sleeps, and only then is the listener used.
#[test]
fn lir_io_22_handle_survives_a_suspension() {
    let source = r#"
import std::net;

async fn exchange(message: String) -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    await sleep(1);
    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    await sleep(1);
    (await net::write_async(client, message))?;
    let server = (await accepting)?;
    let got = (await net::read_async(server, 4096))?;
    return Ok(got);
}

async fn main() {
    match await exchange("after parking") {
        Ok(text) => println(text),
        Err(error) => println("net failed"),
    }
}
"#;
    assert_io(source, "after parking\n", &["exchange", "main"]);
}

// 23. a void `?` immediately before a suspension: the success arm continues
//     into the park, so the block the `?` left must be the one that reaches it.
#[test]
fn lir_io_23_void_propagation_before_a_suspension() {
    let source = r#"
import std::fs;

async fn write_then_park(path: String) -> Result<String, IoError> {
    (await fs::write_string_async(path, "parked"))?;
    await sleep(1);
    let back = (await fs::read_to_string_async(path))?;
    await yield();
    (await fs::remove_file_async(path))?;
    return Ok(back);
}

async fn main() {
    let path = fs::temp_path("willow_lir_io_23");
    match await write_then_park(path) {
        Ok(text) => println(text),
        Err(error) => println("io failed"),
    }
}
"#;
    assert_io(source, "parked\n", &["write_then_park", "main"]);
}

// 24. the shipped example is fully walker-compiled: every function in it, not
//     just the ones a focused test happens to name.
#[test]
fn lir_io_24_example_is_fully_lir() {
    let source = std::fs::read_to_string("example/lir_io.wi").expect("the example is readable");
    assert_io(
        &source,
        "on disk\nfalse\ntrue\nwritten by a task\nfalse\nhello over loopback\n",
        &[
            "roundtrip",
            "read_missing",
            "is_failure",
            "async_roundtrip",
            "greet",
            "serve",
            "loopback",
            "main",
        ],
    );
}
