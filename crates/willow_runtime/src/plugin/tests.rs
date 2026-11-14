use super::*;
use willow_abi::plugin::{HEADER_BYTES, NATIVE_ENGINE, VERSION, WASM_ENGINE};
fn wasm_bytes(body: &str) -> Vec<u8> {
    wat::parse_str(format!("(module (memory (export \"memory\") 1 2) (func (export \"call\") (param i32 i32 i32 i64) (result i64) {body}))")).unwrap()
}
fn manifest(bytes: &[u8]) -> Manifest {
    Manifest {
        version: VERSION,
        digest: digest(bytes),
        mode: Mode::Wasm,
        engine: WASM_ENGINE.into(),
        target: "wasm32".into(),
        methods: (1..=24).collect(),
        read_resources: vec![],
        budgets: Budgets::default(),
    }
}
fn start(bytes: &[u8]) -> Session {
    Session::start(manifest(bytes), bytes, HashMap::new()).unwrap()
}
fn echo() -> Vec<u8> {
    wasm_bytes("local.get 2 i64.extend_i32_u")
}
fn call(s: &mut Session, method: u32, bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let (id, _) = s.submit(method, None, bytes).unwrap();
    assert!(s.drive_one());
    s.take(id).unwrap()
}
#[test]
fn wasm_round_trip() {
    assert_eq!(
        call(&mut start(&echo()), 1, b"hello"),
        Ok(b"hello".to_vec())
    );
}
#[test]
fn digest_rejected_before_execution() {
    let b = echo();
    let mut m = manifest(&b);
    m.digest = "0".repeat(64);
    assert!(matches!(
        Session::start(m, &b, HashMap::new()),
        Err(Error::DigestMismatch)
    ));
}
#[test]
fn engine_and_protocol_fail_closed() {
    let b = echo();
    for field in 0..3 {
        let mut m = manifest(&b);
        match field {
            0 => m.version += 1,
            1 => m.engine = "unknown".into(),
            _ => m.target = "unknown".into(),
        };
        assert!(Session::start(m, &b, HashMap::new()).is_err());
    }
}
#[test]
fn wasm_trap_fails_all_pending_once() {
    let mut s = start(&wasm_bytes("unreachable"));
    let (a, _) = s.submit(1, None, b"").unwrap();
    let (b, _) = s.submit(1, None, b"").unwrap();
    s.drive_one();
    assert_eq!(s.take(a), Some(Err(Error::Trap)));
    assert_eq!(s.take(b), Some(Err(Error::Trap)));
    assert!(s.take(a).is_none());
    assert_eq!(s.counters().1, 2);
}
#[test]
fn wasm_fuel_is_bounded() {
    let mut s = start(&wasm_bytes("(loop $l br $l) i64.const 0"));
    assert_eq!(call(&mut s, 1, b""), Err(Error::ResourceLimit));
}
#[test]
fn wasm_memory_growth_is_bounded() {
    let b = wasm_bytes("i32.const 0 i32.const 3 memory.grow i32.store i64.const 4");
    assert_eq!(call(&mut start(&b), 1, b""), Ok(vec![255; 4]));
}
#[test]
fn wasm_initial_memory_is_bounded() {
    let b = echo();
    let mut m = manifest(&b);
    m.budgets.memory_bytes = 65536;
    let b2 = wat::parse_str("(module (memory 3))").unwrap();
    m.digest = digest(&b2);
    assert!(Session::start(m, &b2, HashMap::new()).is_err());
}
#[test]
fn wasm_offsets_and_lengths_are_checked() {
    for packed in [u64::MAX, 0xfffffff000000020, 65535u64 << 32 | 2] {
        let b = wasm_bytes(&format!("i64.const {}", packed as i64));
        assert!(call(&mut start(&b), 1, b"").is_err());
    }
}
#[test]
fn wasm_host_escape_imports_are_denied() {
    for (module, name) in [
        ("wasi_snapshot_preview1", "path_open"),
        ("wasi_snapshot_preview1", "sock_open"),
        ("env", "system"),
        ("willow", "resource_read"),
    ] {
        let b =
            wat::parse_str(format!("(module (import \"{module}\" \"{name}\" (func)))")).unwrap();
        assert!(matches!(
            Session::start(manifest(&b), &b, HashMap::new()),
            Err(Error::DeniedCapability)
        ));
    }
}
#[test]
fn wasm_resource_snapshot_and_denial() {
    let b=wat::parse_str("(module (import \"willow\" \"resource_read\" (func $read (param i64 i32 i32) (result i32))) (memory (export \"memory\") 1) (func (export \"call\") (param i32 i32 i32 i64) (result i64) local.get 3 i32.const 0 i32.const 3 call $read i64.extend_i32_u))").unwrap();
    let mut m = manifest(&b);
    m.read_resources = vec!["config".into()];
    let mut s = Session::start(m, &b, HashMap::from([("config".into(), b"abc".to_vec())])).unwrap();
    let (id, _) = s.submit(1, s.resource("config"), b"").unwrap();
    s.drive_one();
    assert_eq!(s.take(id), Some(Ok(b"abc".to_vec())));
    assert_eq!(call(&mut s, 1, b""), Err(Error::DeniedCapability));
}
#[test]
fn methods_are_checked_before_queueing() {
    let mut s = start(&echo());
    assert!(matches!(
        s.submit(99, None, b""),
        Err(Error::DeniedCapability)
    ));
    assert_eq!(s.counters(), (0, 0, 0, 0));
}
#[test]
fn backpressure_includes_completed_responses() {
    let b = echo();
    let mut m = manifest(&b);
    m.budgets.requests = 1;
    let mut s = Session::start(m, &b, HashMap::new()).unwrap();
    let (id, _) = s.submit(1, None, b"a").unwrap();
    assert!(matches!(s.submit(1, None, b""), Err(Error::ResourceLimit)));
    s.drive_one();
    assert!(s.submit(1, None, b"").is_err());
    s.take(id);
    assert!(s.submit(1, None, b"").is_ok());
}
#[test]
fn aggregate_bytes_are_reserved_before_copy() {
    let b = echo();
    let mut m = manifest(&b);
    m.budgets.queued_bytes = m.budgets.payload_bytes;
    let mut s = Session::start(m, &b, HashMap::new()).unwrap();
    assert!(matches!(s.submit(1, None, b"a"), Err(Error::ResourceLimit)));
    assert_eq!(s.counters().3, 0);
}
#[test]
fn queued_cancel_does_not_dispatch_or_kill_session() {
    let mut s = start(&echo());
    let (id, c) = s.submit(1, None, b"a").unwrap();
    assert!(c.cancel());
    assert!(!c.cancel());
    s.drive_one();
    assert_eq!(s.take(id), Some(Err(Error::Cancelled)));
    assert_eq!(s.state(), State::Running);
    assert_eq!(s.counters().0, 0);
}
#[test]
fn completed_response_wins_late_cancellation() {
    let mut s = start(&echo());
    let (id, c) = s.submit(1, None, b"a").unwrap();
    s.drive_one();
    assert!(!c.cancel());
    assert_eq!(s.take(id), Some(Ok(b"a".to_vec())));
}
#[test]
fn queued_deadline_does_not_dispatch() {
    let mut s = start(&echo());
    let (id, _) = s.submit(1, None, b"").unwrap();
    s.pending.get_mut(&id).unwrap().deadline = Instant::now();
    s.drive_one();
    assert_eq!(s.take(id), Some(Err(Error::Timeout)));
    assert_eq!(s.counters().0, 0);
}
#[test]
fn running_wasm_cancellation_and_timeout() {
    for timeout in [false, true] {
        let b = wasm_bytes("(loop $l br $l) i64.const 0");
        let mut m = manifest(&b);
        m.budgets.fuel = 1_000_000_000;
        m.budgets.deadline_ms = if timeout { 10 } else { 1000 };
        let mut s = Session::start(m, &b, HashMap::new()).unwrap();
        let (id, c) = s.submit(1, None, b"").unwrap();
        std::thread::scope(|scope| {
            if !timeout {
                scope.spawn(crate::runtime::serving(move || {
                    std::thread::sleep(Duration::from_millis(2));
                    c.cancel();
                }));
            }
            s.drive_one();
        });
        assert_eq!(
            s.take(id),
            Some(Err(if timeout {
                Error::Timeout
            } else {
                Error::Cancelled
            }))
        );
    }
}
#[test]
fn shutdown_revokes_resources_and_resolves_once() {
    let mut s = start(&echo());
    let (id, _) = s.submit(1, None, b"").unwrap();
    s.shutdown();
    s.shutdown();
    assert_eq!(s.take(id), Some(Err(Error::Cancelled)));
    assert!(!s.drive_one());
    assert!(matches!(s.submit(1, None, b""), Err(Error::Stopped)));
    assert_eq!(s.counters().1, 1);
}
#[test]
fn restarted_instance_rejects_old_identifiers() {
    let b = echo();
    let mut first = start(&b);
    let (old, _) = first.submit(1, None, b"").unwrap();
    first.shutdown();
    let mut next = start(&b);
    assert!(next.take(old).is_none());
    let stale = ResourceId {
        generation: first.generation,
        number: 1,
    };
    assert!(matches!(
        next.submit(1, Some(stale), b""),
        Err(Error::DeniedCapability)
    ));
}
#[test]
fn manifest_resource_bounds() {
    let b = echo();
    let mut m = manifest(&b);
    m.read_resources = vec!["config".into()];
    m.budgets.resource_bytes = 2;
    assert!(matches!(
        Session::start(m, &b, HashMap::from([("config".into(), vec![0; 3])])),
        Err(Error::ResourceLimit)
    ));
}
#[test]
fn deterministic_protocol_mutations_never_allocate_from_untrusted_length() {
    let h = Header {
        tag: Tag::Response,
        method: 1,
        generation: 1,
        request: 1,
        resource: 0,
        length: 1,
    };
    for index in 0..HEADER_BYTES {
        for value in 0..=255 {
            let mut bytes = h.encode();
            bytes[index] = value;
            let parsed = Header::decode(&bytes, 64);
            if let Ok(p) = parsed {
                assert!(p.length <= 64);
                assert_eq!(Header::decode(&p.encode(), 64), Ok(p));
            }
        }
    }
    let mut bytes = h.encode();
    bytes[40..48].fill(255);
    assert_eq!(Header::decode(&bytes, 64), Err(Error::ResourceLimit));
}
#[test]
fn indexed_completion_scales_with_request_count() {
    for count in [1, 16, 128, 512] {
        let b = echo();
        let mut m = manifest(&b);
        m.budgets.requests = count;
        m.budgets.payload_bytes = 8;
        m.budgets.queued_bytes = count * 16;
        let mut s = Session::start(m, &b, HashMap::new()).unwrap();
        let ids: Vec<_> = (0..count)
            .map(|_| s.submit(1, None, b"x").unwrap().0)
            .collect();
        while s.drive_one() {}
        for id in ids.into_iter().rev() {
            assert_eq!(s.take(id), Some(Ok(b"x".to_vec())));
        }
        assert_eq!(s.counters(), (count as u64, count as u64, 0, 0));
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn native() -> Option<Session> {
    use std::sync::OnceLock;
    static BINARY: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    let binary = BINARY.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("willow-plugin-{}", std::process::id()));
        let status = std::process::Command::new("cc")
            .args(["-static", "-O2", "-o"])
            .arg(&path)
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/examples/plugin/guest.c"
            ))
            .status()
            .ok()?;
        let bytes = if status.success() {
            std::fs::read(&path).ok()
        } else {
            None
        };
        let _ = std::fs::remove_file(path);
        bytes
    });
    let result = match binary {
        Some(b) => {
            let mut m = manifest(b);
            m.mode = Mode::NativeProcess;
            m.engine = NATIVE_ENGINE.into();
            m.target = "x86_64-unknown-linux-gnu".into();
            m.read_resources = vec!["config".into()];
            Session::start(m, b, HashMap::from([("config".into(), b"secret".to_vec())]))
        }
        None => Err(Error::IsolationUnavailable),
    };
    match result {
        Ok(s) => Some(s),
        Err(Error::IsolationUnavailable)
            if std::env::var_os("WILLOW_REQUIRE_NATIVE_ISOLATION").is_none() =>
        {
            eprintln!("native backend unavailable on this host");
            None
        }
        Err(e) => panic!("native backend: {e:?}"),
    }
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_round_trip_and_resource_access() {
    let Some(mut s) = native() else { return };
    assert_eq!(call(&mut s, 1, b"hello"), Ok(b"hello".to_vec()));
    let (id, _) = s.submit(5, s.resource("config"), b"").unwrap();
    s.drive_one();
    assert_eq!(s.take(id), Some(Ok(b"secret".to_vec())));
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_actual_escape_sentinels_and_memory_limit() {
    let Some(mut s) = native() else { return };
    let path = std::env::temp_dir().join(format!("willow-escape-{}", std::process::id()));
    std::fs::write(&path, b"unchanged").unwrap();
    assert_eq!(
        call(&mut s, 4, path.to_str().unwrap().as_bytes()),
        Ok(vec![63])
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
    std::fs::remove_file(path).unwrap();
    assert_eq!(call(&mut s, 12, b""), Ok(vec![1]));
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_malformed_frames_and_crash_fail_once() {
    for (method, error) in [
        (2, Error::Crash),
        (6, Error::ResourceLimit),
        (7, Error::ProtocolMismatch),
        (8, Error::ProtocolMismatch),
        (9, Error::ProtocolMismatch),
        (10, Error::Crash),
        (11, Error::ProtocolMismatch),
        (19, Error::ProtocolMismatch),
        (20, Error::ProtocolMismatch),
        (21, Error::Crash),
        (22, Error::Trap),
        (23, Error::ResourceLimit),
        (24, Error::ProtocolMismatch),
    ] {
        let Some(mut s) = native() else { return };
        let (a, _) = s.submit(method, None, b"").unwrap();
        let (b, _) = s.submit(1, None, b"").unwrap();
        s.drive_one();
        assert_eq!(s.take(a), Some(Err(error)));
        assert_eq!(s.take(b), Some(Err(error)));
        assert_eq!(s.counters().1, 2);
        assert_eq!(s.state(), State::Failed);
    }
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_fragmented_frames_round_trip() {
    let Some(mut s) = native() else { return };
    assert_eq!(call(&mut s, 13, &vec![7; 4096]), Ok(vec![7; 4096]));
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_cancellation_terminates_and_reaps() {
    let Some(mut s) = native() else { return };
    let (id, c) = s.submit(3, None, b"").unwrap();
    std::thread::scope(|scope| {
        scope.spawn(crate::runtime::serving(move || {
            std::thread::sleep(Duration::from_millis(10));
            c.cancel();
        }));
        s.drive_one();
    });
    assert_eq!(s.take(id), Some(Err(Error::Cancelled)));
    assert!(s.backend.is_none());
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_deadline_terminates_and_reaps() {
    let Some(mut s) = native() else { return };
    let (id, _) = s.submit(3, None, b"").unwrap();
    s.pending.get_mut(&id).unwrap().deadline = Instant::now() + Duration::from_millis(10);
    s.drive_one();
    assert_eq!(s.take(id), Some(Err(Error::Timeout)));
    assert!(s.backend.is_none());
}
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
#[test]
fn foreign_native_backend_fails_closed() {
    let b = echo();
    let mut m = manifest(&b);
    m.mode = Mode::NativeProcess;
    m.engine = NATIVE_ENGINE.into();
    m.target = "x86_64-unknown-linux-gnu".into();
    assert!(matches!(
        Session::start(m, &b, HashMap::new()),
        Err(Error::IsolationUnavailable)
    ));
}

#[test]
fn manifest_json_is_bounded_and_strict() {
    assert!(matches!(
        Manifest::from_json(&vec![0; 65537]),
        Err(Error::ResourceLimit)
    ));
    let m = manifest(&echo());
    let bytes = serde_json::to_vec(&m).unwrap();
    assert!(Manifest::from_json(&bytes).is_ok());
    let mut value = serde_json::to_value(m).unwrap();
    value["allow_network"] = true.into();
    assert!(Manifest::from_json(&serde_json::to_vec(&value).unwrap()).is_err());
}
#[test]
fn wasm_callbacks_are_deferred_and_bounded() {
    for count in [1, 17] {
        let b=wat::parse_str(format!("(module (import \"willow\" \"callback\" (func $callback (param i32 i32) (result i32))) (memory (export \"memory\") 1) (func (export \"call\") (param i32 i32 i32 i64) (result i64) {} i64.const 0))", "i32.const 0 i32.const 1 call $callback drop ".repeat(count))).unwrap();
        let mut s = start(&b);
        let (id, _) = s.submit(1, None, b"x").unwrap();
        assert!(s.take_callback(id).is_none());
        s.drive_one();
        let mut callbacks = 0;
        while let Some(bytes) = s.take_callback(id) {
            assert_eq!(bytes, b"x");
            callbacks += 1;
        }
        assert_eq!(callbacks, count.min(16));
        assert_eq!(s.take(id), Some(Ok(vec![])));
    }
}
#[test]
fn wasm_callback_and_response_share_byte_budget() {
    let b=wat::parse_str("(module (import \"willow\" \"callback\" (func $callback (param i32 i32) (result i32))) (memory (export \"memory\") 1) (func (export \"call\") (param i32 i32 i32 i64) (result i64) i32.const 0 i32.const 1 call $callback drop i64.const 1))").unwrap();
    let mut m = manifest(&b);
    m.budgets.payload_bytes = 1;
    let mut s = Session::start(m, &b, HashMap::new()).unwrap();
    let (id, _) = s.submit(1, None, b"x").unwrap();
    s.drive_one();
    assert!(s.take_callback(id).is_none());
    assert_eq!(s.take(id), Some(Err(Error::ResourceLimit)));
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_callbacks_are_deferred_and_bounded() {
    let Some(mut s) = native() else { return };
    let (id, _) = s.submit(14, None, b"event").unwrap();
    assert!(s.take_callback(id).is_none());
    s.drive_one();
    assert_eq!(s.take_callback(id), Some(b"event".to_vec()));
    assert_eq!(s.take(id), Some(Ok(vec![])));
    assert_eq!(call(&mut s, 15, b""), Err(Error::ResourceLimit));
}

#[test]
fn cancellation_response_race_has_one_winner() {
    for _ in 0..32 {
        let mut s = start(&echo());
        let (id, token) = s.submit(1, None, b"value").unwrap();
        let barrier = std::sync::Barrier::new(2);
        let won = std::thread::scope(|scope| {
            let worker = scope.spawn(crate::runtime::serving(|| {
                barrier.wait();
                token.cancel()
            }));
            barrier.wait();
            s.drive_one();
            worker.join().unwrap()
        });
        let result = s.take(id).unwrap();
        assert_eq!(
            result,
            if won {
                Err(Error::Cancelled)
            } else {
                Ok(b"value".to_vec())
            }
        );
        assert!(s.take(id).is_none());
        assert_eq!(s.counters().1, 1);
    }
}
#[test]
fn resource_reuse_after_restart_never_revives_old_handle() {
    let b = echo();
    let mut m = manifest(&b);
    m.read_resources = vec!["config".into()];
    let mut first =
        Session::start(m.clone(), &b, HashMap::from([("config".into(), vec![1])])).unwrap();
    let old = first.resource("config").unwrap();
    first.shutdown();
    assert!(first.resource("config").is_none());
    let mut second = Session::start(m, &b, HashMap::from([("config".into(), vec![2])])).unwrap();
    assert!(matches!(
        second.submit(1, Some(old), b""),
        Err(Error::DeniedCapability)
    ));
    assert!(second.submit(1, second.resource("config"), b"").is_ok());
}

#[test]
fn wasm_compilation_limits_reject_adverse_function_fanout() {
    let bytes = wat::parse_str(format!("(module {})", "(func)".repeat(10001))).unwrap();
    assert!(Session::start(manifest(&bytes), &bytes, HashMap::new()).is_err());
}
#[test]
fn wasm_host_copy_work_is_bounded_separately_from_instructions() {
    let bytes = wat::parse_str("(module (import \"willow\" \"resource_read\" (func $read (param i64 i32 i32) (result i32))) (memory (export \"memory\") 1) (func (export \"call\") (param i32 i32 i32 i64) (result i64) (loop $again i64.const 1 i32.const 0 i32.const 1000 call $read drop br $again) i64.const 0))").unwrap();
    let mut m = manifest(&bytes);
    m.read_resources = vec!["config".into()];
    m.budgets.fuel = 10000;
    let mut s =
        Session::start(m, &bytes, HashMap::from([("config".into(), vec![0; 1000])])).unwrap();
    assert_eq!(call(&mut s, 1, b""), Err(Error::ResourceLimit));
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_closes_ungranted_inherited_descriptors() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let file = std::fs::File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let raw = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 200) };
    assert!(raw >= 200);
    let inherited = unsafe { OwnedFd::from_raw_fd(raw) };
    let Some(mut s) = native() else {
        return;
    };
    assert_eq!(
        call(&mut s, 16, &inherited.as_raw_fd().to_le_bytes()),
        Ok(vec![1])
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_broken_transport_never_signals_host() {
    if std::env::var_os("WILLOW_PLUGIN_SIGPIPE_CHILD").is_some() {
        let Some(mut session) = native() else {
            return;
        };
        assert_eq!(call(&mut session, 17, b""), Ok(vec![]));
        if let Some(Backend::Native(native)) = session.backend.as_mut() {
            native.wait_for_exit();
        }
        // Only change the disposition inside this dedicated test subprocess.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
        assert_eq!(call(&mut session, 1, b""), Err(Error::Crash));
    } else {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "plugin::tests::native_broken_transport_never_signals_host",
            ])
            .env("WILLOW_PLUGIN_SIGPIPE_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_guest_has_only_protocol_descriptors() {
    let Some(mut s) = native() else {
        return;
    };
    assert_eq!(call(&mut s, 18, b""), Ok(vec![0]));
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_denied_resource_is_a_structured_error() {
    let Some(mut s) = native() else {
        return;
    };
    assert_eq!(call(&mut s, 5, b""), Err(Error::DeniedCapability));
}

#[test]
fn diagnostics_report_revocation_without_guest_text() {
    let b = echo();
    let mut m = manifest(&b);
    m.read_resources = vec!["config".into()];
    let mut s = Session::start(m, &b, HashMap::from([("config".into(), vec![1])])).unwrap();
    assert_eq!(
        s.diagnostics()["read_resources"],
        serde_json::json!(["config"])
    );
    assert_eq!(s.diagnostics()["network"], false);
    s.shutdown();
    assert_eq!(s.diagnostics()["read_resources"], serde_json::json!([]));
    assert_eq!(s.diagnostics()["state"], "Stopped");
}
