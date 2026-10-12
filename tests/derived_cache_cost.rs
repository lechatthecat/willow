//! Reproducible derived-wire boundary cost evidence for willow-r52c.12.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct CountingAllocator;
thread_local! {
    static COUNT: Cell<Option<usize>> = const { Cell::new(None) };
}
fn record() {
    let _ = COUNT.try_with(|count| {
        if let Some(value) = count.get() {
            count.set(Some(value + 1));
        }
    });
}
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record();
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record();
        unsafe { System.alloc_zeroed(layout) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn measured<T>(f: impl FnOnce() -> T) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            COUNT.with(|count| count.set(None));
        }
    }
    COUNT.with(|count| count.set(Some(0)));
    let _reset = Reset;
    let value = f();
    (value, COUNT.with(|count| count.get().unwrap()))
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Record {
    name: String,
    instructions: Vec<u64>,
}

#[test]
fn derived_wire_boundary_cost_scaling() {
    use std::{hint::black_box, time::Instant};
    for size in [16, 64, 256, 1024] {
        let records: Vec<_> = (0..size)
            .map(|n| Record {
                name: format!("body{n}"),
                instructions: vec![n; 16],
            })
            .collect();
        let wire = serde_json::to_vec(&records).unwrap();
        let legacy = || {
            let dom: serde_json::Value = serde_json::from_slice(black_box(&wire)).unwrap();
            let typed: Vec<Record> = serde_json::from_value(dom).unwrap();
            // Unchanged-coordinate LIR remap followed by presentation refresh.
            let dom = serde_json::to_value(&typed).unwrap();
            let typed: Vec<Record> = serde_json::from_value(dom).unwrap();
            let dom = serde_json::to_value(&typed).unwrap();
            let canonical = serde_json::to_vec(&dom).unwrap();
            let wire = serde_json::to_vec(&dom).unwrap();
            black_box((canonical, wire));
            typed
        };
        let direct = || serde_json::from_slice::<Vec<Record>>(black_box(&wire)).unwrap();
        let (old, old_allocs) = measured(legacy);
        let (new, new_allocs) = measured(direct);
        assert_eq!(old, new);
        assert!(new_allocs * 2 < old_allocs);
        let repeats = 50;
        let start = Instant::now();
        for _ in 0..repeats {
            black_box(legacy());
        }
        let old_ns = start.elapsed().as_nanos();
        let start = Instant::now();
        for _ in 0..repeats {
            black_box(direct());
        }
        let new_ns = start.elapsed().as_nanos();
        println!(
            "records={size} hits={repeats} payload_bytes={} legacy_allocs={old_allocs} direct_allocs={new_allocs} legacy_ns={old_ns} direct_ns={new_ns}",
            wire.len()
        );
    }
}
