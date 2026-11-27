use super::*;

#[test]
fn blocking_pool_values_close_and_function_values() {
    let f = Fixture::new();
    f.sources(
        r#"
        import std::collections::Array;
        import native::Resource; import native::resource; import native::value; import native::drops; import native::add; import native::echo; import native::bytes; import native::nested; import native::floats; import native::Resource_close;
        fn relay()->i64 { let f=add; return f(20,22); }
        async fn main() {
            println(relay());
            println(echo("owned input"));
            let b=bytes([1,2]); println(b[1] == 2);
            println(nested(Some("nested")).unwrap());
            println(floats([1.5,2.5])[1] == 2.5);
            let r=resource(); println(value(r)); Resource_close(r);
            println(drops());
        }
    "#,
        r#"
        use std::sync::atomic::{AtomicI64, Ordering};
        static DROPS:AtomicI64=AtomicI64::new(0);
        pub struct Resource;
        impl Drop for Resource { fn drop(&mut self) { DROPS.fetch_add(1,Ordering::SeqCst); } }
        pub fn resource()->Box<Resource> { Box::new(Resource) }
        pub fn value(_: &Resource)->i64 { 42 }
        pub fn drops()->i64 { DROPS.load(Ordering::SeqCst) }
        pub fn add(a:i64,b:i64)->i64 {a+b}
        pub fn echo(s:&str)->String {s.to_owned()}
        pub fn bytes(s:&[u8])->Vec<u8> {s.to_vec()}
        pub fn nested(s:Option<&str>)->Result<String,i64> {s.map(str::to_owned).ok_or(1)}
        pub fn floats(s:Vec<f64>)->Vec<f64> {s}
    "#,
    );
    let output = f.successful(&["run", "--offline"]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "42\nowned input\ntrue\nnested\ntrue\n42\n1\n"
    );
    let query = f.successful(&["query", "rust-bridge", "--format", "json"]);
    let value: serde_json::Value = serde_json::from_slice(&query.stdout).unwrap();
    assert!(
        value["bridges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["execution"] == "blocking_pool_in_task")
    );
}

#[test]
fn blocking_pool_one_worker_progress_pins_gc_and_reviewed_affinity() {
    let f = Fixture::new();
    let rust = format!(
        r#"
        {WAIT_FOR_COLLECTION}
        use std::sync::atomic::{{AtomicBool,Ordering}};
        static STARTED:AtomicBool=AtomicBool::new(false);
        static RELEASED:AtomicBool=AtomicBool::new(false);
        #[willow::assume(no_block)] pub fn started()->bool {{ STARTED.load(Ordering::SeqCst) }}
        #[willow::assume(no_block)] pub fn release() {{ RELEASED.store(true,Ordering::SeqCst); }}
        #[willow::assume(no_block)] pub fn direct()->bool {{ !std::thread::current().name().unwrap_or("").starts_with("willow-rust-blocking") }}
        pub fn hold(s:&str)->String {{
            assert!(std::thread::current().name().unwrap_or("").starts_with("willow-rust-blocking"));
            STARTED.store(true,Ordering::SeqCst);
            let deadline=std::time::Instant::now()+std::time::Duration::from_secs(10);
            while !RELEASED.load(Ordering::SeqCst) {{ assert!(std::time::Instant::now()<deadline,"worker stalled"); std::thread::yield_now(); }}
            wait_for_collection();
            s.to_owned()
        }}
    "#
    );
    f.reviewed_sources(
        r#"
        import native::hold; import native::started; import native::release; import native::direct;
        async fn held()->String { return hold("survives collection"); }
        async fn other() { while !started() { await yield(); } println(direct()); release(); }
        async fn main() { let h=held(); let o=other(); await o; println(await h); }
    "#,
        &rust,
    );
    let output = f.command_env(
        &["run", "--offline"],
        &[("WILLOW_WORKERS", "1"), ("WILLOW_BLOCKING_THREADS", "1")],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "true\nsurvives collection\n"
    );
}

#[test]
fn blocking_pool_panic_recovers_and_pool_is_reused() {
    let f = Fixture::new();
    f.sources(r#"
        import native::fail; import native::answer;
        fn caught() { defer match recover() { Some(info)=>println(info.message), None=>{} } fail(); println("unreachable"); }
        async fn main() { caught(); println(answer()); caught(); println(answer()); }
    "#,r#"pub fn fail() { panic!("pool panic"); } pub fn answer()->i64 {42}"#);
    let output = f.command_env(
        &["run", "--offline"],
        &[("WILLOW_WORKERS", "1"), ("WILLOW_BLOCKING_THREADS", "1")],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "RustPanic: pool panic\n42\nRustPanic: pool panic\n42\n"
    );
}

#[test]
fn blocking_pool_contracts_and_lock_diagnostics() {
    let f = Fixture::new();
    for (body, accepted) in [
        (
            "@effects(no_block) fn relay() { touch(); } async fn main() {relay();}",
            true,
        ),
        (
            "@effects(no_wait) fn relay() { touch(); } fn main() {}",
            false,
        ),
        (
            "@effects(no_suspend) fn relay() { touch(); } fn main() {}",
            false,
        ),
        (
            "fn relay() { touch(); } @effects(no_wait) fn checked() {relay();} fn main() {}",
            false,
        ),
        (
            "async fn main() { let m=Mutex::new(0); lock m as x {touch();} }",
            false,
        ),
        (
            "fn relay() { touch(); } async fn main() { let m=Mutex::new(0); lock m as x {relay();} }",
            false,
        ),
    ] {
        f.sources(
            &format!("import native::touch; {body}"),
            "pub fn touch() {}",
        );
        let output = f.command(&["check", "--offline"]);
        let error = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), accepted, "{body}: {error}");
        if !accepted {
            assert!(error.contains("nonblocking"), "{body}: {error}");
        }
    }
}

#[test]
fn blocking_pool_rejects_thread_local_opaque_types() {
    let f = Fixture::new();
    for native in [
        "pub struct Local(std::rc::Rc<i64>); pub fn local()->Box<Local> {Box::new(Local(std::rc::Rc::new(42)))}",
        "pub struct Local(std::cell::Cell<i64>); pub fn local()->Box<Local> {Box::new(Local(std::cell::Cell::new(42)))}",
    ] {
        f.sources("import native::Local; import native::local; import native::Local_close; async fn main() {let x=local(); defer Local_close(x);}",native);
        let output = f.command(&["build", "--offline"]);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "non-transferable type accepted");
        assert!(
            error.contains("cannot be shared") || error.contains("cannot be sent"),
            "{error}"
        );
    }
}

#[test]
fn blocking_pool_wrapper_counts_are_linear_and_reviewed_has_no_dispatch() {
    for count in [1, 16, 256] {
        let symbols: Vec<_> = (0..count)
            .map(|i| RustBridgeSymbol::new(format!("f{i}"), vec![Scalar::I64], Scalar::I64))
            .collect();
        let source = wrappers(&symbols, Path::new("ffi.rs"), 1);
        assert_eq!(
            source.matches("blocking_call(move || adapter(").count(),
            count
        );
        assert_eq!(source.matches("fn blocking_call<").count(), 1);
        let mut reviewed = symbols;
        for s in &mut reviewed {
            s.declare_no_block();
            s.apply_review(true);
        }
        let direct = wrappers(&reviewed, Path::new("ffi.rs"), 1);
        assert!(!direct.contains("blocking_call"));
        assert_eq!(
            direct
                .matches("let _region = BridgeRegion::enter()")
                .count(),
            count
        );
        eprintln!(
            "declarations={count} pool_dispatch={count} shared_transport=1 direct_dispatch=0 generated_bytes={}",
            source.len()
        );
    }
}

#[test]
fn blocking_pool_native_modules_records_and_qualified_defer_capture() {
    let f = Fixture::new();
    fs::write(
        f.root.join("project.toml"),
        "[project]\nname=\"blocking_native\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        f.root.join("src/native.rs"),
        r#"
        use std::sync::atomic::{AtomicI64, Ordering};
        static STORED:AtomicI64=AtomicI64::new(0);
        static DROPS:AtomicI64=AtomicI64::new(0);
        pub struct Message { pub text:String, pub count:i64 }
        pub struct Resource { value:i64 }
        impl Drop for Resource { fn drop(&mut self) { DROPS.fetch_add(1,Ordering::SeqCst); } }
        pub fn make(s:&str)->Message { Message {text:s.to_owned(), count:42} }
        pub fn echo(m:Message)->Message {m}
        pub fn resource()->Box<Resource> {Box::new(Resource{value:42})}
        pub fn value(r:&Resource)->i64 {r.value}
        pub fn store(n:i64) {STORED.store(n,Ordering::SeqCst);}
        pub fn stored()->i64 {STORED.load(Ordering::SeqCst)}
        pub fn drops()->i64 {DROPS.load(Ordering::SeqCst)}
    "#,
    )
    .unwrap();
    fs::write(
        f.root.join("src/main.wi"),
        r#"
        import native;
        async fn scoped() {
            let r=native::resource(); defer native::Resource_close(r);
            println(native::value(r));
            let mut n=1; defer native::store(n); n=2;
            await yield();
            println(n);
        }
        async fn main() {
            println(native::echo(native::make("雪🙂")).text);
            await scoped(); println(native::stored()); println(native::drops());
        }
    "#,
    )
    .unwrap();
    let output = f.command_env(
        &["run", "--offline"],
        &[
            ("WILLOW_WORKERS", "1"),
            ("WILLOW_GC_STRESS", "alloc,relocate"),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "雪🙂\n42\n2\n1\n1\n"
    );
}

#[test]
fn blocking_pool_cancel_runs_unreviewed_close_and_destructor_gc_region() {
    let f = Fixture::new();
    let rust = format!(
        r#"
        {WAIT_FOR_COLLECTION}
        use std::sync::atomic::{{AtomicBool,AtomicI64,Ordering}};
        static CREATED:AtomicBool=AtomicBool::new(false);
        static DROPS:AtomicI64=AtomicI64::new(0);
        pub struct Resource;
        impl Drop for Resource {{ fn drop(&mut self) {{
            assert!(std::thread::current().name().unwrap_or("").starts_with("willow-rust-blocking"));
            wait_for_collection(); DROPS.fetch_add(1,Ordering::SeqCst);
        }} }}
        pub fn resource()->Box<Resource> {{Box::new(Resource)}}
        #[willow::assume(no_block)] pub fn mark() {{CREATED.store(true,Ordering::SeqCst);}}
        #[willow::assume(no_block)] pub fn created()->bool {{CREATED.load(Ordering::SeqCst)}}
        pub fn drops()->i64 {{DROPS.load(Ordering::SeqCst)}}
    "#
    );
    f.reviewed_sources(r#"
        import native::Resource; import native::resource; import native::created; import native::mark; import native::drops; import native::Resource_close;
        async fn hold() { let r=resource(); defer Resource_close(r); mark(); while true {await yield();} }
        async fn main() { let h=hold(); while !created() {await yield();} h.cancel(); await h.result(); println(drops()); }
    "#,&rust);
    let output = f.command_env(&["run", "--offline"], &[("WILLOW_WORKERS", "1")]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "1\n");
}

#[test]
fn blocking_pool_aggregate_and_destructor_panics_release_then_recover() {
    let f = Fixture::new();
    f.sources(r#"
        import native::Resource; import native::resource; import native::fail; import native::drops; import native::Resource_close;
        fn aggregate() { defer match recover() {Some(info)=>println(info.message),None=>{}} fail("owned"); println("unreachable"); }
        fn close() { let r=resource(); defer match recover() {Some(info)=>println(info.message),None=>{}} Resource_close(r); println("unreachable"); }
        async fn main() { aggregate(); close(); println(drops()); aggregate(); }
    "#,r#"
        use std::sync::atomic::{AtomicI64,Ordering};
        static DROPS:AtomicI64=AtomicI64::new(0);
        pub struct Resource;
        impl Drop for Resource {fn drop(&mut self) {DROPS.fetch_add(1,Ordering::SeqCst); panic!("drop panic");}}
        pub fn resource()->Box<Resource> {Box::new(Resource)}
        pub fn fail(s:&str)->String {assert_eq!(s,"owned"); panic!("aggregate panic");}
        pub fn drops()->i64 {DROPS.load(Ordering::SeqCst)}
    "#);
    let output = f.command_env(
        &["run", "--offline"],
        &[
            ("WILLOW_WORKERS", "1"),
            ("WILLOW_BLOCKING_THREADS", "1"),
            ("WILLOW_GC_STRESS", "alloc,relocate"),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "RustPanic: aggregate panic\nRustPanic: drop panic\n1\nRustPanic: aggregate panic\n"
    );
}

#[test]
fn blocking_pool_panic_payload_destructor_stays_in_gc_region() {
    let f = Fixture::new();
    let native = format!(
        r#"
        {WAIT_FOR_COLLECTION}
        use std::sync::atomic::{{AtomicUsize,Ordering}};
        static DROPS:AtomicUsize=AtomicUsize::new(0);
        struct Secondary;
        impl Drop for Secondary {{ fn drop(&mut self) {{
            wait_for_collection(); DROPS.fetch_add(1,Ordering::SeqCst);
        }} }}
        struct Payload;
        impl Drop for Payload {{ fn drop(&mut self) {{
            assert!(std::thread::current().name().unwrap_or("").starts_with("willow-rust-blocking"));
            wait_for_collection();
            DROPS.fetch_add(1,Ordering::SeqCst);
            std::panic::panic_any(Secondary);
        }} }}
        pub fn fail() {{ std::panic::panic_any(Payload); }}
        pub fn drops()->i64 {{ DROPS.load(Ordering::SeqCst) as i64 }}
    "#
    );
    f.sources(r#"
        import native::fail; import native::drops;
        fn caught() { defer match recover() {Some(info)=>println(info.message),None=>{}} fail(); println("unreachable"); }
        async fn main() { caught(); println(drops()); caught(); println(drops()); }
    "#, &native);
    let output = f.command_env(
        &["run", "--offline"],
        &[("WILLOW_WORKERS", "1"), ("WILLOW_BLOCKING_THREADS", "1")],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "RustPanic: non-string panic payload\n2\nRustPanic: non-string panic payload\n4\n"
    );
}
