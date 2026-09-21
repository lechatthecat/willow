use super::*;

#[test]
fn net_01_loopback_echo_uses_async_socket_operations() {
    // Request five workers explicitly. Non-blocking behavior is pinned
    // separately by readiness parking/wake and cancellation tests.
    let (out, ok) = compile_and_run_with_env(NET_ECHO_SOURCE, &[("WILLOW_WORKERS", "5")]);
    assert!(ok, "{out}");
    assert_eq!(out, "ping\n");
}

#[test]
fn net_02_loopback_echo_survives_gc_stress() {
    let (out, ok) = compile_and_run_with_env(
        NET_ECHO_SOURCE,
        &[
            ("WILLOW_WORKERS", "5"),
            ("WILLOW_GC_STRESS", "alloc"),
            ("WILLOW_TASK_BUDGET", "1"),
        ],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "ping\n");
}

#[test]
fn net_03_module_alias_dispatches_to_builtin() {
    let source = NET_ECHO_SOURCE
        .replace("import std::net;", "import std::net as network;")
        .replace("net::", "network::");
    let (out, ok) = compile_and_run(&source);
    assert!(ok, "{out}");
    assert_eq!(out, "ping\n");
}

#[test]
fn net_04_invalid_address_is_typed_error() {
    let (out, ok) = compile_and_run(
        "import std::net;\nasync fn main() { match await net::connect_async(\"localhost:80\") { Ok(stream) => println(\"connected\"), Err(error) => println(\"invalid\"), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "invalid\n");
}

#[test]
fn net_05_cancelled_accept_deregisters_and_listener_can_be_reused() {
    let (out, ok) = compile_and_run_with_env(
        r#"
import std::net;

async fn run() -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let cancelled = net::accept_async(listener);
    await sleep(5);
    cancelled.cancel();
    match await cancelled.result() {
        Ok(stream) => println("unexpected"),
        Err(Cancelled) => println("cancelled"),
    }

    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    (await net::write_async(client, "after-cancel"))?;
    let server = (await accepting)?;
    return await net::read_async(server, 1024);
}

async fn main() {
    match await run() {
        Ok(text) => println(text),
        Err(error) => println("network error"),
    }
}
"#,
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cancelled\nafter-cancel\n");
}

#[test]
fn net_06_timer_wakes_while_accept_is_parked() {
    let (out, ok) = compile_and_run(
        r#"
import std::net;

async fn run() -> Result<void, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let accepting = net::accept_async(listener);
    await sleep(5);
    println("timer");
    accepting.cancel();
    return Ok();
}

async fn main() {
    match await run() {
        Ok(value) => println("done"),
        Err(error) => println("network error"),
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "timer\ndone\n");
}

#[test]
fn net_07_read_bound_validation_is_result_error() {
    let source = NET_ECHO_SOURCE.replace(
        "net::read_async(server, 1024)",
        "net::read_async(server, 0)",
    );
    let (out, ok) = compile_and_run(&source);
    assert!(ok, "{out}");
    assert_eq!(out, "network error\n");
}

#[test]
fn net_08_empty_write_completes_successfully() {
    let (out, ok) = compile_and_run(
        r#"
import std::net;

async fn run() -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    (await net::write_async(client, ""))?;
    net::shutdown(client)?;
    let server = (await accepting)?;
    return await net::read_async(server, 1);
}

async fn main() {
    match await run() {
        Ok(text) => println(text),
        Err(error) => println("network error"),
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "\n");
}

#[test]
fn net_09_imported_handle_types_accept_annotations() {
    let (out, ok) = compile_and_run(
        r#"
import std::net;
import std::net::TcpListener;

fn check_listener() -> Result<bool, IoError> {
    let listener: TcpListener = net::bind("127.0.0.1:0")?;
    let value = net::local_addr(listener)?;
    return Ok(value != "");
}

fn main() {
    match check_listener() {
        Ok(value) => println(value),
        Err(error) => println(false),
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn net_10_wrong_handle_type_is_rejected() {
    assert_compile_error_contains(
        "import std::net;\nasync fn main() { let task = net::accept_async(1); }",
        &["error[E0201]", "expected `TcpListener`, found `i64`"],
    );
}

#[test]
fn net_12_repeated_shutdown_is_successful() {
    let source = NET_ECHO_SOURCE
        .replace(
            "net::shutdown(client)?;",
            "net::shutdown(client)?; net::shutdown(client)?;",
        )
        .replace(
            "net::shutdown(server)?;",
            "net::shutdown(server)?; net::shutdown(server)?;",
        );
    let (out, ok) = compile_and_run(&source);
    assert!(ok, "{out}");
    assert_eq!(out, "ping\n");
}

#[test]
fn net_11_utf8_payload_roundtrips() {
    let source = NET_ECHO_SOURCE.replace("\"ping\"", "\"日本語🌱\"");
    let (out, ok) = compile_and_run_with_env(&source, &[("WILLOW_WORKERS", "5")]);
    assert!(ok, "{out}");
    assert_eq!(out, "日本語🌱\n");
}
