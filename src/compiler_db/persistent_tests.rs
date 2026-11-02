use super::*;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "willow-persistent-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture: {error}"),
            }
        }
    }
    fn inputs(&self) -> CompilerInputs {
        CompilerInputs::native(crate::CompilerOptions::debug(), self.0.clone())
    }
    fn key(&self) -> CacheKey {
        CacheKey::new(&self.inputs(), &self.0.join("main.wi")).unwrap()
    }
    fn compile(
        &self,
        source: &str,
        old: Option<&DiskRevision>,
        syntax: SyntaxQueries,
    ) -> (Frontend, Rc<RefCell<SyntaxQueries>>) {
        let entry = self.0.join("main.wi");
        std::fs::write(&entry, source).unwrap();
        let map = SourceMap::new(entry.to_str().unwrap(), source);
        let syntax = Rc::new(RefCell::new(syntax.candidate().unwrap()));
        let frontend = crate::run_frontend_revision_cached(
            source,
            &self.0,
            &map,
            self.inputs(),
            &mut Diagnostics::default(),
            None,
            Some(Rc::clone(&syntax)),
            old,
        )
        .unwrap();
        (frontend, syntax)
    }
    fn saved(&self) -> PathBuf {
        let (frontend, syntax) = self.compile(SOURCE, None, SyntaxQueries::default());
        let path = self.0.join("analysis.cache");
        save(&path, self.key(), &frontend, &syntax.borrow()).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Default)]
struct Diagnostics(Vec<serde_json::Value>);
impl DiagnosticEmitter for Diagnostics {
    fn emit(
        &mut self,
        diagnostic: &crate::diagnostics::Diagnostic,
        _: &dyn crate::diagnostics::source_map::SourceLookup,
    ) -> std::io::Result<()> {
        self.0.push(serde_json::to_value(diagnostic).unwrap());
        Ok(())
    }
}
const SOURCE: &str = "fn first() -> i64 { return 1; } fn second() -> i64 { return first(); } fn main() { let value = second(); }";
fn envelope(payload: &[u8]) -> Vec<u8> {
    let mut bytes = digest(payload).into_bytes();
    bytes.push(b'\n');
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn persistent_cache_compatibility_components_reject_independently() {
    let fixture = Fixture::new();
    let path = fixture.saved();
    assert!(load(&path, &fixture.key()).is_ok());
    for (field, replacement) in [
        ("format", serde_json::json!(FORMAT + 1)),
        ("compiler", serde_json::json!("different-compiler")),
        ("target", serde_json::json!("different-target")),
        ("runtime_abi", serde_json::json!(0)),
        (
            "std_schema",
            serde_json::json!("different-standard-library"),
        ),
        ("configuration", serde_json::json!([])),
    ] {
        let mut key = serde_json::to_value(fixture.key()).unwrap();
        key[field] = replacement;
        let key = serde_json::from_value(key).unwrap();
        let error = load(&path, &key).err().expect(field);
        assert!(
            error.to_string().contains("incompatible"),
            "{field}: {error}"
        );
    }
}

#[test]
fn persistent_cache_configuration_tracks_independent_inputs() {
    let fixture = Fixture::new();
    let key = fixture.key();
    let entry = fixture.0.join("main.wi");
    let mut inputs = fixture.inputs();
    inputs.options.worker_count = Some(17);
    assert!(CacheKey::new(&inputs, &entry).unwrap() != key);
    let mut inputs = fixture.inputs();
    inputs.options.enforce_send_sync = !inputs.options.enforce_send_sync;
    assert!(CacheKey::new(&inputs, &entry).unwrap() != key);
    let mut inputs = fixture.inputs();
    inputs.target.sync_stack_preemption = !inputs.target.sync_stack_preemption;
    assert!(CacheKey::new(&inputs, &entry).unwrap() != key);
    assert!(CacheKey::new(&fixture.inputs(), &fixture.0.join("other.wi")).unwrap() != key);
    std::fs::write(fixture.0.join("project.toml"), "first manifest").unwrap();
    let manifest_key = fixture.key();
    assert!(manifest_key != key);
    std::fs::write(fixture.0.join("project.toml"), "second manifest").unwrap();
    assert!(fixture.key() != manifest_key);
    std::fs::remove_file(fixture.0.join("project.toml")).unwrap();
    std::fs::write(fixture.0.join("project.lock"), "lock bytes").unwrap();
    assert!(fixture.key() != key);
    std::fs::remove_file(fixture.0.join("project.lock")).unwrap();
    assert!(fixture.key() == key);
}

#[test]
fn persistent_cache_corrupt_records_are_misses_and_can_be_replaced() {
    let fixture = Fixture::new();
    let path = fixture.saved();
    let valid = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(load(&path, &fixture.key()).is_err());
    let mut broken_checksum = valid.clone();
    broken_checksum[0] = if broken_checksum[0] == b'a' {
        b'b'
    } else {
        b'a'
    };
    let mut broken_separator = valid.clone();
    broken_separator[64] = b' ';
    let mut broken_payload = valid.clone();
    *broken_payload.last_mut().unwrap() ^= 1;
    for (name, bytes) in [
        ("empty", vec![]),
        ("short header", valid[..32].to_vec()),
        ("header only", valid[..65].to_vec()),
        ("truncated payload", valid[..valid.len() - 1].to_vec()),
        ("checksum", broken_checksum),
        ("separator", broken_separator),
        ("payload", broken_payload),
        ("invalid json", envelope(b"not-json")),
        ("missing fields", envelope(b"{}")),
    ] {
        std::fs::write(&path, bytes).unwrap();
        assert!(load(&path, &fixture.key()).is_err(), "accepted {name}");
    }
    let original: serde_json::Value = serde_json::from_slice(&valid[65..]).unwrap();
    for id in [0, u32::MAX] {
        let mut malformed = original.clone();
        malformed["modules"][0][0] = serde_json::json!(id);
        std::fs::write(&path, envelope(&serde_json::to_vec(&malformed).unwrap())).unwrap();
        assert!(
            load(&path, &fixture.key()).is_err(),
            "accepted invalid module ID"
        );
    }
    let mut duplicate = original.clone();
    let module = duplicate["modules"][0].clone();
    duplicate["modules"].as_array_mut().unwrap().push(module);
    std::fs::write(&path, envelope(&serde_json::to_vec(&duplicate).unwrap())).unwrap();
    assert!(
        load(&path, &fixture.key()).is_err(),
        "accepted duplicate module ID"
    );
    for field in ["id_ends", "parsed", "index", "bodies", "syntax"] {
        let mut malformed = original.clone();
        malformed[field] = serde_json::Value::Null;
        std::fs::write(&path, envelope(&serde_json::to_vec(&malformed).unwrap())).unwrap();
        assert!(
            load(&path, &fixture.key()).is_err(),
            "accepted invalid {field}"
        );
    }
    for input in [
        serde_json::json!({"SignatureSymbol": {"Dispatch": [0, "method"]}}),
        serde_json::json!("Target"),
    ] {
        let mut malformed = original.clone();
        let checks = malformed["bodies"]["checks"].as_array_mut().unwrap();
        let signature = checks
            .iter_mut()
            .find(|check| check[0][1] == "Signature")
            .unwrap();
        signature[1]["inputs"] = serde_json::json!([[input, null]]);
        std::fs::write(&path, envelope(&serde_json::to_vec(&malformed).unwrap())).unwrap();
        assert!(
            load(&path, &fixture.key()).is_err(),
            "accepted foreign signature dependency"
        );
    }
    let mut exhausted = original.clone();
    exhausted["syntax"]["table"]["revision"] = serde_json::json!(u64::MAX);
    std::fs::write(&path, envelope(&serde_json::to_vec(&exhausted).unwrap())).unwrap();
    assert!(load(&path, &fixture.key()).is_err());
    let mut invalid_range = original;
    invalid_range["id_ends"] = serde_json::json!([0, 0]);
    std::fs::write(
        &path,
        envelope(&serde_json::to_vec(&invalid_range).unwrap()),
    )
    .unwrap();
    assert!(load(&path, &fixture.key()).is_err());
    crate::project::init::atomic_replace(&path, &valid).unwrap();
    assert!(load(&path, &fixture.key()).is_ok());
}

#[test]
fn persistent_cache_noop_and_single_body_edit_reuse_counts() {
    let fixture = Fixture::new();
    let path = fixture.saved();
    let (previous, syntax) = load(&path, &fixture.key()).unwrap();
    let (warm, syntax) = fixture.compile(SOURCE, Some(&previous), syntax);
    assert_eq!(warm.db.typed_bodies.typechecks(), 0);
    assert!(warm.db.typed_bodies.reused() >= 3);
    save(&path, fixture.key(), &warm, &syntax.borrow()).unwrap();
    let (previous, syntax) = load(&path, &fixture.key()).unwrap();
    let edited = SOURCE.replace("return 1;", "return 9;");
    let (warm, syntax) = fixture.compile(&edited, Some(&previous), syntax);
    assert_eq!(warm.db.typed_bodies.typechecks(), 1);
    assert!(warm.db.typed_bodies.reused() >= 2);
    save(&path, fixture.key(), &warm, &syntax.borrow()).unwrap();
    let (previous, syntax) = load(&path, &fixture.key()).unwrap();
    let (warm, _) = fixture.compile(&edited, Some(&previous), syntax);
    assert_eq!(warm.db.typed_bodies.typechecks(), 0);
    assert!(warm.db.typed_bodies.reused() >= 3);
}

#[test]
fn persistent_cache_concurrent_publication_is_complete() {
    let fixture = Fixture::new();
    let path = fixture.saved();
    let valid = std::fs::read(&path).unwrap();
    let mut payload: serde_json::Value = serde_json::from_slice(&valid[65..]).unwrap();
    payload["key"]["compiler"] = serde_json::json!("other-valid-writer");
    let other = envelope(&serde_json::to_vec(&payload).unwrap());
    std::thread::scope(|scope| {
        for bytes in [&valid, &other] {
            let path = &path;
            scope.spawn(move || {
                for _ in 0..16 {
                    crate::project::init::atomic_replace(path, bytes).unwrap();
                }
            });
        }
        for _ in 0..64 {
            let read = std::fs::read(&path).unwrap();
            assert!(read == valid || read == other);
            assert_eq!(&read[..64], digest(&read[65..]).as_bytes());
        }
    });
    let read = std::fs::read(&path).unwrap();
    assert!(read == valid || read == other);
    crate::project::init::atomic_replace(&path, &valid).unwrap();
    assert!(load(&path, &fixture.key()).is_ok());
}

#[test]
fn semantic_verify_persistent_cache_readonly_reuse_counts() {
    for count in [8, 32, 128] {
        let fixture = Fixture::new();
        let mut source = String::from(SOURCE);
        for i in 0..count {
            source.push_str(&format!(" fn unrelated{i}() -> i64 {{ return {i}; }}"));
        }
        let entry = fixture.0.join("main.wi");
        std::fs::write(&entry, &source).unwrap();
        let map = SourceMap::new(entry.to_str().unwrap(), &source);
        run(
            &source,
            &fixture.0,
            &map,
            fixture.inputs(),
            &mut Diagnostics::default(),
        )
        .unwrap();
        let path = cache_path(&fixture.0, &entry);
        let cache_before = std::fs::read(&path).unwrap();
        let edited = source.replacen("return 1;", "return 9;", 1);
        std::fs::write(&entry, &edited).unwrap();
        let map = SourceMap::new(entry.to_str().unwrap(), &edited);
        let mut inputs = fixture.inputs();
        inputs.capture_analysis = true;
        inputs.verification = true;
        let warm = run(
            &edited,
            &fixture.0,
            &map,
            inputs,
            &mut Diagnostics::default(),
        )
        .unwrap();
        println!(
            "semantic_verify_cache unrelated={count} typechecks={} reused={}",
            warm.db.typed_bodies.typechecks(),
            warm.db.typed_bodies.reused()
        );
        assert_eq!(warm.db.typed_bodies.typechecks(), 1);
        assert_eq!(warm.db.typed_bodies.signature_checks.get(), 0);
        assert_eq!(warm.db.typed_bodies.concurrency_checks.get(), 1);
        assert_eq!(
            warm.db.effects.work().1,
            0,
            "value-only edit must reuse solved effects"
        );
        assert!(warm.db.typed_bodies.reused() >= count + 2);
        assert_eq!(std::fs::read(&path).unwrap(), cache_before);
        let snapshot = crate::ai::snapshot(&warm, &entry, &edited, None).unwrap();
        assert!(snapshot.functions.iter().any(|f| f.name == "first"));
    }
}

#[test]
fn semantic_verify_disk_named_and_generic_signatures_reuse_and_clean_parity() {
    for count in [8, 32, 128] {
        for (declaration, ty) in [
            ("class Box {}", "Box"),
            ("class Box<T> { value: T; }", "Box<i64>"),
            ("enum Box<T> { Value(T) }", "Box<i64>"),
            ("enum Box<T> { Value(T) }", "fn(Box<i64>) -> Box<i64>"),
        ] {
            let fixture = Fixture::new();
            let compile = |text: &str, old: Option<&DiskRevision>, syntax: SyntaxQueries| {
                let entry = fixture.0.join("main.wi");
                std::fs::write(&entry, text).unwrap();
                let map = SourceMap::new(entry.to_str().unwrap(), text);
                let syntax = Rc::new(RefCell::new(syntax.candidate().unwrap()));
                let mut inputs = fixture.inputs();
                inputs.capture_analysis = true;
                inputs.verification = true;
                let frontend = crate::run_frontend_revision_cached(
                    text,
                    &fixture.0,
                    &map,
                    inputs,
                    &mut Diagnostics::default(),
                    None,
                    Some(Rc::clone(&syntax)),
                    old,
                )
                .unwrap();
                (frontend, syntax)
            };
            let mut source = format!("{SOURCE} {declaration}");
            for i in 0..count {
                source.push_str(&format!(
                    " fn unrelated{i}(v: {ty}) -> {ty} {{ return v; }}"
                ));
            }
            let (before, syntax) = compile(&source, None, SyntaxQueries::default());
            let path = fixture.0.join("analysis.cache");
            save(&path, fixture.key(), &before, &syntax.borrow()).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let (disk, syntax) = load(&path, &fixture.key()).unwrap();
            let edited = source.replacen("return 1;", "return 9;", 1);
            let (warm, _) = compile(&edited, Some(&disk), syntax);
            assert_eq!(
                warm.db.typed_bodies.signature_checks.get(),
                0,
                "{ty} {count}"
            );
            let (clean, _) = compile(&edited, None, SyntaxQueries::default());
            let entry = fixture.0.join("main.wi");
            let baseline = |frontend, text: &str| {
                let snapshot = crate::ai::snapshot(frontend, &entry, text, None).unwrap();
                crate::ai::verify::Baseline::from_snapshot(&snapshot, vec![]).unwrap()
            };
            let base = baseline(&before, &source);
            assert_eq!(
                serde_json::to_value(base.compare(&baseline(&warm, &edited)).unwrap()).unwrap(),
                serde_json::to_value(base.compare(&baseline(&clean, &edited)).unwrap()).unwrap(),
                "{ty} {count}",
            );
            assert_eq!(std::fs::read(path).unwrap(), bytes);
            assert_eq!(std::fs::read_to_string(entry).unwrap(), edited);
        }
    }
}

#[test]
fn persistent_precheck_malformed_keys_and_outputs_are_cache_misses() {
    let fixture = Fixture::new();
    let (frontend, syntax) = fixture.compile(
        "class Box<T> { value: T; } fn use_box(v: Box<i64>) -> Box<i64> { return v; } fn main() {}",
        None,
        SyntaxQueries::default(),
    );
    let path = fixture.0.join("analysis.cache");
    save(&path, fixture.key(), &frontend, &syntax.borrow()).unwrap();
    let valid = std::fs::read(&path).unwrap();
    let original: serde_json::Value = serde_json::from_slice(&valid[65..]).unwrap();
    assert!(!original["prechecks"].as_array().unwrap().is_empty());
    for dependency in [
        serde_json::json!({"field":"symbols","key":null}),
        serde_json::json!({"field":"symbols","key":{"Dispatch":[0,"m"]}}),
        serde_json::json!({"field":"unknown","key":"bad"}),
        serde_json::json!({"field":"generic.instances","key":"bad"}),
    ] {
        let mut candidate = original.clone();
        candidate["prechecks"][0][1]["reads"] = serde_json::json!([[dependency, null]]);
        std::fs::write(&path, envelope(&serde_json::to_vec(&candidate).unwrap())).unwrap();
        assert!(load(&path, &fixture.key()).is_err());
    }
    let mut candidate = original;
    candidate["prechecks"][0][1]["writes"] =
        serde_json::json!([[{"field":"symbols","key":{"Class":"Box"}},"bad"]]);
    std::fs::write(&path, envelope(&serde_json::to_vec(&candidate).unwrap())).unwrap();
    assert!(load(&path, &fixture.key()).is_err());
    std::fs::write(&path, valid).unwrap();
    assert!(load(&path, &fixture.key()).is_ok());
}

#[test]
fn readonly_analysis_cache_overlay_scaling_and_input_validation() {
    for count in [8, 32, 128] {
        let fixture = Fixture::new();
        let entry = fixture.0.join("main.wi");
        let mut source = String::from(SOURCE);
        // Fan-out: callers depend on the same leaf, but a value-only edit does
        // not change its signature or force any caller to be rechecked.
        for i in 0..count {
            source.push_str(&format!(" fn caller{i}() -> i64 {{ return first(); }}"));
        }
        std::fs::write(&entry, &source).unwrap();
        let compile = |text: &str, analysis: bool| {
            let mut inputs = fixture.inputs();
            inputs.capture_analysis = analysis;
            run(
                text,
                &fixture.0,
                &SourceMap::new(entry.to_str().unwrap(), text),
                inputs,
                &mut Diagnostics::default(),
            )
        };
        compile(&source, false).unwrap();
        let path = cache_path(&fixture.0, &entry);
        let bytes = std::fs::read(&path).unwrap();
        let unchanged = compile(&source, true).unwrap();
        assert_eq!(unchanged.db.typed_bodies.typechecks(), 0);
        assert_eq!(unchanged.db.typed_bodies.reused(), count + 3);
        let edited = source.replacen("return 1;", "return 9;", 1);
        {
            let _overlay = super::super::captured::SourceOverlay::enter(
                [(entry.clone(), edited.clone())].into_iter().collect(),
            );
            let candidate = compile(&edited, true).unwrap();
            assert_eq!(candidate.db.typed_bodies.typechecks(), 1);
            assert_eq!(candidate.db.typed_bodies.reused(), count + 2);
            println!(
                "readonly_fanout bodies={} unchanged_typechecks=0 edited_typechecks=1 reused={} cache_bytes={}",
                count + 3,
                candidate.db.typed_bodies.reused(),
                bytes.len(),
            );
        }
        assert_eq!(std::fs::read_to_string(&entry).unwrap(), source);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            compile(&source, true).unwrap().db.typed_bodies.typechecks(),
            0
        );
        // A stale record cannot hide an invalid isolated candidate body.
        let invalid = source.replacen("return 1;", "return unknown_name;", 1);
        let _overlay = super::super::captured::SourceOverlay::enter(
            [(entry.clone(), invalid.clone())].into_iter().collect(),
        );
        assert!(compile(&invalid, true).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn persistent_build_captures_reference_facts_without_query_source_views() {
    let fixture = Fixture::new();
    let source = "fn id<T>(value: T) -> T { return value; } fn main() { println(id(42)); }";
    let entry = fixture.0.join("main.wi");
    std::fs::write(&entry, source).unwrap();
    let map = SourceMap::new(entry.to_str().unwrap(), source);
    let built = run(
        source,
        &fixture.0,
        &map,
        fixture.inputs(),
        &mut Diagnostics::default(),
    )
    .unwrap();
    assert!(built.db.generic_sources.is_empty());
    assert!(!built.db.inputs().capture_analysis);
    assert!(built.db.inputs().cache_analysis);
    let mut inputs = fixture.inputs();
    inputs.capture_analysis = true;
    let queried = run(
        source,
        &fixture.0,
        &map,
        inputs,
        &mut Diagnostics::default(),
    )
    .unwrap();
    assert!(!queried.db.generic_sources.is_empty());
    assert_eq!(queried.db.typed_bodies.typechecks(), 0);
    let snapshot = crate::ai::snapshot(&queried, &entry, source, None).unwrap();
    assert!(snapshot.functions.iter().any(|f| f.name == "id"));
}
