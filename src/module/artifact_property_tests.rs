//! Deterministic generated properties for the actual on-disk body store.
use super::*;
use std::fmt::Write as _;

fn parse(source: &str, file: FileId) -> Program {
    let tokens = crate::lexer::Lexer::with_file_id(source, file)
        .tokenize()
        .unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}\n{source}");
    program
}

// Linear-size chains, wide bodies, repeated expressions, and Unicode payloads.
// No dependency on random state: the case number completely reproduces a source.
fn source(case: usize, owners: usize, depth: usize) -> String {
    let mut source = String::new();
    for owner in 0..owners {
        let value = case * 100 + owner;
        let mut body = format!("let mut x = {value};");
        for _ in 0..depth {
            body.push_str("if true {");
        }
        match case % 4 {
            0 => body.push_str("let f = |n: i64| { return n + 1; }; x = f(x);"),
            1 => body.push_str("let a = [1, 2, 3]; x = a[0]; defer { println(x); }"),
            2 => body.push_str("x = match true { true => 1, false => 2 };"),
            _ => body.push_str("println(\"日本語 🦀 \\n \\\"\"); while false { break; }"),
        }
        for _ in 0..depth {
            body.push('}');
        }
        for _ in 0..case % 5 {
            body.push_str("x = x + 1;");
        }
        // Five separately stored artifacts per group. Nested syntax belongs to
        // its enclosing body, and must not become extra top-level records.
        writeln!(source, "fn f{owner}() {{ {body} }}").unwrap();
        writeln!(
            source,
            "class C{owner} {{
                x: i64;
                pub static value: i64 = {value} + 1;
                pub init(self) {{ self.x = {value}; {body} }}
                pub fn get(self) -> i64 {{ {body} return self.x; }}
            }}
            interface I{owner} {{
                fn required(self) -> i64;
                fn defaultValue(self) -> i64 {{ {body} return {value}; }}
            }}"
        )
        .unwrap();
    }
    source
}

#[test]
fn generated_body_roundtrips_preserve_identity_and_reuse_records() {
    for case in 0..32 {
        let mut artifacts = UnitArtifacts::new().unwrap();
        let owners = 1 + case % 4;
        let mut summary = parse(&source(case, owners, case % 8), FileId(17));
        let expected = serde_json::to_vec(&summary).unwrap();
        artifacts.offload(&mut summary).unwrap();
        let shell = serde_json::to_vec(&summary).unwrap();
        assert_eq!(artifacts.entries.borrow().len(), 5 * owners, "case {case}");
        for _ in 0..3 {
            let restored = artifacts.hydrate(&summary, FileId(17)).unwrap();
            assert_eq!(
                serde_json::to_vec(&*restored).unwrap(),
                expected,
                "case {case}"
            );
            // Offloading an already stripped shell must not overwrite its bodies.
            artifacts.offload(&mut summary).unwrap();
            assert_eq!(serde_json::to_vec(&summary).unwrap(), shell);
            // Nor should a fresh hydrated copy allocate duplicate artifacts.
            let mut copy = (*restored).clone();
            artifacts.offload(&mut copy).unwrap();
            assert_eq!(serde_json::to_vec(&copy).unwrap(), shell);
            assert_eq!(artifacts.entries.borrow().len(), 5 * owners, "case {case}");
        }
        assert_eq!(artifacts.metrics.borrow().live, [0; 4]);
    }
}

#[test]
fn generated_multifile_roundtrips_have_exact_linear_record_counts() {
    for files in [1, 4, 16] {
        for owners in [1, 4, 16] {
            let mut artifacts = UnitArtifacts::new().unwrap();
            let mut summaries = Vec::new();
            for index in 0..files {
                let file = FileId(index as u32);
                // Equal source offsets across files must not alias. Values vary
                // by file, while identifier spellings deliberately repeat.
                let source = source(index + 10, owners, 2);
                let mut summary = parse(&source, file);
                let expected = serde_json::to_vec(&summary).unwrap();
                artifacts.snapshot_source(file, &source).unwrap();
                artifacts
                    .snapshot_source(file, "replacement must be ignored")
                    .unwrap();
                artifacts.offload(&mut summary).unwrap();
                summaries.push((file, source, summary, expected));
            }
            assert_eq!(artifacts.blocks.len(), files * owners * 4);
            assert_eq!(artifacts.initializers.len(), files * owners);
            assert_eq!(artifacts.entries.borrow().len(), files * (5 * owners + 1));
            // Reverse order detects accidental dependence on the most recently
            // parsed program or sequential artifact reads.
            for (file, source, summary, expected) in summaries.iter().rev() {
                let restored = artifacts.hydrate(summary, *file).unwrap();
                assert_eq!(serde_json::to_vec(&*restored).unwrap(), *expected);
                assert_eq!(artifacts.source(*file).unwrap(), *source);
            }
            assert_eq!(artifacts.peaks(), [1, 0, 0, 0]);
            assert_eq!(artifacts.metrics.borrow().live, [0; 4]);
        }
    }
}

#[test]
fn malformed_body_artifacts_return_errors_and_release_storage() {
    for bytes in [b"".as_slice(), b"{", b"null", b"{}", b"\xff"] {
        let directory;
        {
            let mut artifacts = UnitArtifacts::new().unwrap();
            directory = artifacts.directory.clone();
            let mut summary = parse("fn f() { println(42); }", FileId(3));
            artifacts.offload(&mut summary).unwrap();
            let id = *artifacts.blocks.values().next().unwrap();
            let mut packed = artifacts.pack.borrow_mut();
            let pack = packed.as_mut().unwrap();
            pack.writer.flush().unwrap();
            pack.writer.get_mut().set_len(0).unwrap();
            pack.writer.seek(SeekFrom::Start(0)).unwrap();
            pack.writer.write_all(bytes).unwrap();
            drop(packed);
            artifacts.entries.borrow_mut()[id] = (0, bytes.len() as u64);
            assert!(artifacts.hydrate(&summary, FileId(3)).is_err());
            assert_eq!(artifacts.metrics.borrow().live, [0; 4]);
        }
        assert!(!directory.exists());
    }
}

#[test]
fn identical_spans_in_one_file_have_independent_body_artifacts() {
    let mut artifacts = UnitArtifacts::new().unwrap();
    let mut first = parse("fn f() { println(11); }", FileId(9));
    let mut second = parse("fn f() { println(22); }", FileId(9));
    let expected_first = serde_json::to_vec(&first).unwrap();
    let expected_second = serde_json::to_vec(&second).unwrap();
    artifacts.offload(&mut first).unwrap();
    artifacts.offload(&mut second).unwrap();
    assert_eq!(artifacts.blocks.len(), 2);
    assert_eq!(
        serde_json::to_vec(&*artifacts.hydrate(&first, FileId(9)).unwrap()).unwrap(),
        expected_first
    );
    assert_eq!(
        serde_json::to_vec(&*artifacts.hydrate(&second, FileId(9)).unwrap()).unwrap(),
        expected_second
    );
    assert_eq!(std::fs::read_dir(&artifacts.directory).unwrap().count(), 1);
}

#[test]
fn instantiated_default_hydration_preserves_semantic_ids_and_source_payload() {
    let file = FileId(19);
    let mut summary = parse(
        "interface I { fn get(self) -> i64 { let outer = |x: i64| { let inner = |y: i64| y + x; return inner(x); }; return outer(21); } } class A { pub fn get(self) -> i64 { return 0; } } class B { pub fn get(self) -> i64 { return 0; } }",
        file,
    );
    let Item::Interface(interface) = &summary.items[0] else {
        panic!()
    };
    let source = interface.methods[0].default_body.as_ref().unwrap().clone();
    let mut artifacts = UnitArtifacts::new().unwrap();
    artifacts.offload(&mut summary).unwrap();
    let Item::Interface(interface) = &summary.items[0] else {
        panic!()
    };
    let shell = interface.methods[0].default_body.as_ref().unwrap().clone();
    assert!(shell.stmts.is_empty());
    for item in &mut summary.items[1..] {
        let Item::Class(class) = item else { panic!() };
        class.methods[0].body = shell.clone();
    }
    let unit = crate::module::ModuleId(23);
    artifacts.body_index_mut().register_unit(&mut summary, unit);
    let expected_ids: Vec<_> = summary.items[1..]
        .iter()
        .map(|item| {
            let Item::Class(class) = item else { panic!() };
            class.methods[0].body.id
        })
        .collect();
    assert_ne!(expected_ids[0], expected_ids[1]);
    assert_ne!(expected_ids[0], source.id);
    assert_ne!(expected_ids[1], source.id);
    let stored_count = artifacts.entries.borrow().len();
    let shell_bytes = serde_json::to_vec(&summary).unwrap();
    for _ in 0..3 {
        let restored = artifacts.hydrate(&summary, FileId(23)).unwrap();
        for (item, id) in restored.items[1..].iter().zip(&expected_ids) {
            let Item::Class(class) = item else { panic!() };
            let actual = &class.methods[0].body;
            let mut expected = source.clone();
            expected.id = *id;
            assert_eq!(
                serde_json::to_vec(actual).unwrap(),
                serde_json::to_vec(&expected).unwrap()
            );
            assert_eq!(actual.span.file_id, file);
            assert_eq!(artifacts.bodies.source_body(actual.id), source.id);
        }
        assert_eq!(artifacts.entries.borrow().len(), stored_count);
        assert_eq!(serde_json::to_vec(&summary).unwrap(), shell_bytes);
    }
    assert_eq!(artifacts.metrics.borrow().live, [0; 4]);
}

#[test]
fn query_record_read_buffer_is_bounded_across_fragmented_sizes() {
    let artifacts = UnitArtifacts::new().unwrap();
    let sizes = [0, 31, 40_000, 65_534, 200_000, 7, 65_535];
    let records: Vec<_> = sizes
        .iter()
        .map(|&size| {
            let value = "x".repeat(size);
            (artifacts.write(&value).unwrap(), value)
        })
        .collect();
    for _ in 0..3 {
        for (record, expected) in records.iter().rev() {
            assert_eq!(artifacts.read::<String>(*record).unwrap(), *expected);
            assert_windows_bounded(&artifacts);
        }
    }
}

#[test]
fn batched_query_appends_keep_offsets_across_reads_and_failed_serialization() {
    use serde::ser::SerializeTuple;
    struct PartialFailure;
    impl serde::Serialize for PartialFailure {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut tuple = serializer.serialize_tuple(2)?;
            tuple.serialize_element(&42)?;
            Err(serde::ser::Error::custom("intentional partial record"))
        }
    }

    let artifacts = UnitArtifacts::new().unwrap();
    let mut records = Vec::new();
    for value in 0..128_u64 {
        records.push(artifacts.write(&value).unwrap());
    }
    {
        let packed = artifacts.pack.borrow();
        let pack = packed.as_ref().unwrap();
        // Small consecutive writes stay buffered rather than flushing each key.
        assert!(pack.written > 0);
        assert_eq!(pack.writer.buffer().len() as u64, pack.written);
        assert_eq!(pack.reader.metadata().unwrap().len(), 0);
    }
    for (value, &record) in records.iter().enumerate().rev() {
        assert_eq!(artifacts.read::<u64>(record).unwrap(), value as u64);
    }
    assert!(artifacts.write(&PartialFailure).is_err());
    assert_eq!(artifacts.entries.borrow().len(), records.len());
    let large = "x".repeat(128 * 1024);
    let large_record = artifacts.write(&large).unwrap();
    // Reads move only the reader offset; appending after any read is safe.
    assert_eq!(artifacts.read::<u64>(records[0]).unwrap(), 0);
    let last = artifacts.write(&"last").unwrap();
    assert_eq!(artifacts.read::<String>(large_record).unwrap(), large);
    assert_eq!(artifacts.read::<String>(last).unwrap(), "last");
    for (value, &record) in records.iter().enumerate() {
        assert_eq!(artifacts.read::<u64>(record).unwrap(), value as u64);
    }
}

#[test]
fn sequential_small_record_reads_fill_windows_linearly_in_bytes() {
    // Many small records read in pack order must cost O(bytes / window) file
    // reads, not one seek/read pair per record, for every record count.
    for count in [64_usize, 1_024, 8_192] {
        let artifacts = UnitArtifacts::new().unwrap();
        let records: Vec<_> = (0..count)
            .map(|index| {
                let value = format!("{index:0>150}");
                (artifacts.write(&value).unwrap(), value)
            })
            .collect();
        let bytes = artifacts.store.written();
        for _ in 0..2 {
            for (record, expected) in &records {
                assert_eq!(artifacts.read::<String>(*record).unwrap(), *expected);
            }
        }
        let fills = artifacts.store.read_windows.borrow().fills;
        // In-order reads fill each aligned block once, plus one unaligned
        // window per record that crosses a block boundary.
        let per_pass = 2 * bytes.div_ceil(64 * 1024) as usize;
        assert!(
            fills <= 2 * per_pass,
            "count={count} fills={fills} per_pass={per_pass}"
        );
    }
}

#[test]
fn interleaved_reads_and_appends_decode_every_fragmented_record() {
    // Deterministic LCG: record sizes straddle the write buffer, the read
    // window, and the streaming threshold while reads interleave with appends.
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = |bound: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (state >> 33) % bound
    };
    let artifacts = UnitArtifacts::new().unwrap();
    let mut records: Vec<(usize, String)> = Vec::new();
    for step in 0..2_000 {
        let size = match next(10) {
            0 => 64 * 1024 - 2 + next(5) as usize,
            1 => 70_000 + next(100_000) as usize,
            2..=4 => next(4_096) as usize,
            _ => next(200) as usize,
        };
        let value: String =
            std::iter::repeat_n(char::from(b'a' + (step % 26) as u8), size).collect();
        records.push((artifacts.write(&value).unwrap(), value));
        for _ in 0..next(4) {
            let (record, expected) = &records[next(records.len() as u64) as usize];
            assert_eq!(artifacts.read::<String>(*record).unwrap(), *expected);
        }
    }
    for (record, expected) in records.iter().rev() {
        assert_eq!(artifacts.read::<String>(*record).unwrap(), *expected);
    }
    assert_windows_bounded(&artifacts);
}

fn assert_windows_bounded(artifacts: &UnitArtifacts) {
    let cache = artifacts.store.read_windows.borrow();
    assert!(cache.windows.len() <= 4);
    for window in &cache.windows {
        assert!(window.bytes.capacity() <= 64 * 1024);
    }
}
