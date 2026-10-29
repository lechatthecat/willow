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
    for field in ["id_ends", "parsed", "index", "bodies", "syntax"] {
        let mut malformed = original.clone();
        malformed[field] = serde_json::Value::Null;
        std::fs::write(&path, envelope(&serde_json::to_vec(&malformed).unwrap())).unwrap();
        assert!(
            load(&path, &fixture.key()).is_err(),
            "accepted invalid {field}"
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
        assert!(warm.db.typed_bodies.reused() >= count + 2);
        assert_eq!(std::fs::read(&path).unwrap(), cache_before);
        let snapshot = crate::ai::snapshot(&warm, &entry, &edited, None).unwrap();
        assert!(snapshot.functions.iter().any(|f| f.name == "first"));
    }
}
