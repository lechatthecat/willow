//! Focused snapshot IO benchmark/round-trip proof. Run with --nocapture.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Instant;
use willow_compiler::{CompilerOptions, CompilerSession, ai::Snapshot, diagnostics::HumanEmitter};

struct Allocator;
static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);
fn allocated(delta: isize) {
    let live = LIVE.fetch_add(delta, Ordering::Relaxed) + delta;
    PEAK.fetch_max(live, Ordering::Relaxed);
}
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size() as isize);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(pointer, old, size) };
        if !next.is_null() {
            allocated(size as isize - old.size() as isize);
        }
        next
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn measure<T>(f: impl FnOnce() -> T) -> (T, u128, isize) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let start = Instant::now();
    let result = f();
    let micros = start.elapsed().as_micros();
    (result, micros, PEAK.load(Ordering::Relaxed) - before)
}

#[test]
fn delta_snapshot_io_scaling_and_exact_roundtrip() {
    let directory = std::env::temp_dir().join(format!("willow-delta-io-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let entry = directory.join("main.wi");
    let sizes = if std::env::var_os("WILLOW_DELTA_AUDIT_LARGE").is_some() {
        vec![4096, 16384]
    } else {
        vec![16, 64, 256, 1024]
    };
    for n in sizes {
        let mut source = String::from("fn main() { println(worker0()); }\n");
        for i in 0..n {
            source.push_str(&format!("fn worker{i}() -> i64 {{ return {i}; }}\n"));
        }
        let analyze = || {
            CompilerSession::new(entry.to_str().unwrap(), "", &CompilerOptions::debug(), None)
                .analysis_with_emitter(&mut HumanEmitter)
                .unwrap()
        };
        std::fs::write(&entry, &source).unwrap();
        let before = analyze();
        let base = directory.join(format!("base-{n}.json"));
        before.save(&base).unwrap();
        std::fs::write(&entry, source.replacen("return 0;", "return 1;", 1)).unwrap();
        let after = analyze();
        let full = directory.join(format!("full-{n}.json"));
        let delta = directory.join(format!("delta-{n}.json"));
        let ((), save_full_us, save_full_peak) = measure(|| after.save(&full).unwrap());
        let ((), save_delta_us, save_delta_peak) =
            measure(|| after.save_delta(&delta, &base).unwrap());
        let (loaded, load_full_us, load_full_peak) = measure(|| Snapshot::load(&full).unwrap());
        assert_eq!(loaded.revision, after.revision);
        drop(loaded);
        let (loaded, load_delta_us, load_delta_peak) = measure(|| Snapshot::load(&delta).unwrap());
        assert_eq!(loaded.revision, after.revision);
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&after).unwrap()
        );
        drop(loaded);
        let (pair, pair_full_us, pair_full_peak) =
            measure(|| Snapshot::load_pair(&base, &full).unwrap());
        assert_eq!(pair.1.revision, after.revision);
        drop(pair);
        let (pair, pair_delta_us, pair_delta_peak) =
            measure(|| Snapshot::load_pair(&base, &delta).unwrap());
        assert_eq!(pair.1.revision, after.revision);
        drop(pair);
        let delta_bytes = std::fs::metadata(&delta).unwrap().len();
        // Same-size local edits do not make the delta grow with unrelated bodies.
        assert!(delta_bytes < 4096, "{n}: {delta_bytes}");
        println!(
            "MEASURE {}",
            serde_json::json!({
                "functions":n+1,"source_bytes":source.len(),
                "full_bytes":std::fs::metadata(&full).unwrap().len(),"delta_bytes":delta_bytes,
                "save_full_us":save_full_us,"save_delta_us":save_delta_us,
                "load_full_us":load_full_us,"load_delta_us":load_delta_us,
                "save_full_peak_bytes":save_full_peak,"save_delta_peak_bytes":save_delta_peak,
                "load_full_peak_bytes":load_full_peak,"load_delta_peak_bytes":load_delta_peak,
                "pair_full_us":pair_full_us,"pair_delta_us":pair_delta_us,
                "pair_full_peak_bytes":pair_full_peak,"pair_delta_peak_bytes":pair_delta_peak
            })
        );
    }
    // Changed record shapes must remain exact, including added/deleted functions,
    // symbols/references, control-flow nodes, effect witnesses and source spans.
    let analyze = || {
        CompilerSession::new(entry.to_str().unwrap(), "", &CompilerOptions::debug(), None)
            .analysis_with_emitter(&mut HumanEmitter)
            .unwrap()
    };
    std::fs::write(
        &entry,
        "fn value() -> i64 { return 1; } fn main() { println(value()); }",
    )
    .unwrap();
    let before = analyze();
    let base = directory.join("shapes-base.json");
    before.save(&base).unwrap();
    for (index, source) in [
        "fn value() -> i64 { println(1); return 2; } fn extra() -> bool { return true; } fn main() { if extra() { println(value()); } }",
        "fn main() {}",
        "fn value() -> i64 { return 1; } fn main() { let mut n = 0; while n < 3 { println(value()); n = n + 1; } }",
    ].into_iter().enumerate() {
        std::fs::write(&entry, source).unwrap();
        let after = analyze();
        let delta = directory.join(format!("shape-{index}.json"));
        after.save_delta(&delta, &base).unwrap();
        let loaded = Snapshot::load(&delta).unwrap();
        assert_eq!(serde_json::to_value(&loaded).unwrap(), serde_json::to_value(&after).unwrap());
        assert_eq!(loaded.risk(&before).unwrap(), after.risk(&before).unwrap());
        let bytes = std::fs::read(&delta).unwrap();
        assert!(after.save_delta(&delta, &base).is_err());
        assert_eq!(std::fs::read(&delta).unwrap(), bytes);
    }
    let valid: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("shape-0.json")).unwrap()).unwrap();
    let invalid = directory.join("invalid.json");
    for (field, value) in [
        ("snapshot_encoding", "future"),
        ("base_revision", "wrong"),
        ("revision", "wrong"),
        ("base", "missing.json"),
        ("base", "invalid.json"),
    ] {
        let mut data = valid.clone();
        data[field] = value.into();
        std::fs::write(&invalid, serde_json::to_vec(&data).unwrap()).unwrap();
        assert!(Snapshot::load(&invalid).is_err(), "{field}={value}");
    }
    let mut corrupt = valid.clone();
    corrupt["changes"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"op":"set","path":["functions",0,"name"],"value":"corrupted"}));
    std::fs::write(&invalid, serde_json::to_vec(&corrupt).unwrap()).unwrap();
    assert!(Snapshot::load(&invalid).is_err());
    std::fs::write(&invalid, b"{").unwrap();
    assert!(Snapshot::load(&invalid).is_err());
    let hidden = directory.join("hidden.json");
    std::fs::rename(&base, &hidden).unwrap();
    assert!(Snapshot::load(&directory.join("shape-0.json")).is_err());
    std::fs::rename(&hidden, &base).unwrap();
    assert!(Snapshot::load(&directory.join("shape-0.json")).is_ok());
    assert!(std::fs::read_dir(&directory).unwrap().all(|file| {
        !file
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));
    std::fs::remove_dir_all(directory).unwrap();
}
