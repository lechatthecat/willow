//! Real runtime operations; checksums are independent of scheduling and addresses.
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use willow_runtime::{array as a, channel as c, gc, scheduler as s, string, task};

static COMPLETED: AtomicU64 = AtomicU64::new(0);
static CHECKSUM: AtomicU64 = AtomicU64::new(0);
static OVERLOAD_PEAK: AtomicU64 = AtomicU64::new(0);
pub fn overload_peak() -> u64 {
    OVERLOAD_PEAK.load(Ordering::Relaxed)
}

static CHANNEL_OVERLAP: AtomicU64 = AtomicU64::new(0);
static SWEEP_OVERLAP: AtomicU64 = AtomicU64::new(0);
pub fn overlap_counts() -> (u64, u64) {
    (
        CHANNEL_OVERLAP.load(Ordering::Relaxed),
        SWEEP_OVERLAP.load(Ordering::Relaxed),
    )
}

static CANCELLED: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" {
    fn willow_rust_bridge_frame_new() -> *mut u8;
    fn willow_rust_bridge_frame_drop(frame: *mut u8);
    fn willow_rust_bridge_root(frame: *mut u8, object: u64);
}
// Task frames follow the async-frame ABI: the scheduler publishes terminal
// status into the fixed header, so data (and GC slots) start after it.
#[repr(C)]
struct Frame {
    header: [u64; willow_runtime::async_frame::ASYNC_FRAME_HEADER_WORDS as usize],
    value: u64,
    remaining: u64,
    channel: *mut c_void,
    mode: u64,
}
unsafe extern "C" fn cancel(frame: *mut c_void) {
    let f = unsafe { &*frame.cast::<Frame>() };
    CHECKSUM.fetch_add(f.value, Ordering::Relaxed);
    CANCELLED.fetch_add(1, Ordering::Relaxed);
    COMPLETED.fetch_add(1, Ordering::Release);
}
unsafe extern "C" fn poll(frame: *mut c_void) -> i32 {
    let f = unsafe { &mut *frame.cast::<Frame>() };
    let phase_before = (f.mode != 0 && f.remaining % 64 == 0)
        .then(willow_runtime::gc_telemetry::milestones::snapshot);
    if f.mode == 1 {
        // Producer: bounded channel admission, cooperative retry.
        if c::willow_channel_try_send_i64(f.channel, f.value as i64) != 1 {
            return task::RUNTIME_POLL_YIELD;
        }
    } else if f.mode == 2 {
        // Consumer: readiness API registers real runtime waiters.
        if c::willow_channel_recv_ready(f.channel) == 0 {
            return task::RUNTIME_POLL_YIELD;
        }
        CHECKSUM.fetch_add(
            c::willow_channel_recv_i64(f.channel) as u64,
            Ordering::Relaxed,
        );
    } else {
        CHECKSUM.fetch_add(f.value, Ordering::Relaxed);
    }
    if let Some(before) = phase_before {
        let after = willow_runtime::gc_telemetry::milestones::snapshot();
        if before.epoch == after.epoch && before.stage < 3 && after.stage < 3 {
            CHANNEL_OVERLAP.fetch_add(1, Ordering::Relaxed);
        }
    }
    std::hint::black_box(string::willow_string_alloc(b"request-body".as_ptr(), 12));
    f.value += 1;
    f.remaining -= 1;
    if f.remaining == 0 {
        COMPLETED.fetch_add(1, Ordering::Release);
        task::RUNTIME_POLL_READY
    } else {
        task::RUNTIME_POLL_YIELD
    }
}
fn spawn(value: usize, count: usize, channel: *mut c_void, mode: u64) -> u64 {
    // `channel` is the sole managed reference. Publication installs the scheduler root.
    let channel_word = std::mem::offset_of!(Frame, channel) / 8;
    let frame = gc::willow_alloc_typed(std::mem::size_of::<Frame>() as i64, 1 << channel_word);
    unsafe {
        frame.cast::<Frame>().write(Frame {
            header: [0; willow_runtime::async_frame::ASYNC_FRAME_HEADER_WORDS as usize],
            value: value as u64,
            remaining: count as u64,
            channel,
            mode,
        });
    }
    s::willow_sched_spawn_cooperative(poll, frame.cast()) as u64
}

pub fn tasks(shape: &str, count: usize, workers: usize) -> u64 {
    let collector = (shape == "producer_consumer").then(|| {
        std::thread::spawn(|| {
            let _registration = gc::MutatorRegistration::new();
            for _ in 0..3 {
                gc::willow_gc_collect();
            }
        })
    });
    let mut total = 0;
    // Finite batches bound task-table/frame pressure while still executing millions
    // of independent tasks in the nightly case. Never reuse a completed task ID.
    for start in (0..count).step_by(1024) {
        let n = (count - start).min(1024);
        COMPLETED.store(0, Ordering::Relaxed);
        CHECKSUM.store(0, Ordering::Relaxed);
        CANCELLED.store(0, Ordering::Relaxed);
        if matches!(shape, "pingpong" | "fanin" | "producer_consumer") {
            let mut channel = c::willow_channel_new_bounded(0, 16).cast::<u8>();
            gc::willow_push_root(&mut channel);
            let producers = if shape == "fanin" { workers.min(n) } else { 1 };
            let mut offset = 0;
            for producer in 0..producers {
                let amount = n / producers + usize::from(producer < n % producers);
                spawn(start + offset, amount, channel.cast(), 1);
                offset += amount;
            }
            spawn(0, n, channel.cast(), 2);
            s::willow_sched_run();
            assert_eq!(COMPLETED.load(Ordering::Acquire), producers as u64 + 1);
            c::willow_channel_close(channel.cast());
            gc::willow_pop_root();
        } else {
            for value in start..start + n {
                let id = spawn(value, 1, std::ptr::null_mut(), 0);
                if shape == "cancellation" {
                    s::willow_sched_set_cancel_fn_cooperative(id, cancel, 1);
                    s::willow_sched_cancel(id);
                }
            }
            s::willow_sched_run();
            assert_eq!(COMPLETED.load(Ordering::Acquire), n as u64);
            if shape == "cancellation" {
                assert!(
                    CANCELLED.load(Ordering::Acquire) > 0,
                    "no cancellation executed"
                );
            }
        }
        total += CHECKSUM.load(Ordering::Acquire);
        gc::willow_gc_safepoint();
    }
    if let Some(collector) = collector {
        while !collector.is_finished() {
            gc::willow_gc_safepoint();
            std::thread::yield_now();
        }
        collector.join().unwrap();
    }
    assert_eq!(total, (count as u64) * (count as u64 - 1) / 2);
    total
}

pub fn managed(shape: &str, count: usize) -> u64 {
    let mut root = a::willow_array_new(count as i64, 1);
    gc::willow_push_root(&mut root);
    let mut empty = string::willow_string_alloc(b"!".as_ptr(), 1);
    gc::willow_push_root(&mut empty);
    // These C ABI entry points are the actual bridge loan owner, not a synthetic
    // benchmark pin counter. Its lifetime spans minor and major collections.
    let before = willow_runtime::gc_telemetry::survivor_snapshot();
    let mut pinned_addresses = Vec::new();
    let loan = if shape == "ffi_pins" {
        unsafe { willow_rust_bridge_frame_new() }
    } else {
        std::ptr::null_mut()
    };
    for i in 0..count {
        let text = format!("item-{i:08}");
        let source = string::willow_string_alloc(text.as_ptr(), text.len() as i64);
        let object = string::willow_string_concat(source, empty);
        if !loan.is_null() {
            unsafe { willow_rust_bridge_root(loan, object as u64) };
            pinned_addresses.push(object as usize);
        }
        a::willow_array_set(root, i as i64, object as i64);
        if i % 256 == 0 {
            gc::willow_gc_minor_collect();
        }
    }
    gc::willow_gc_minor_collect();
    gc::willow_gc_collect();
    let mut checksum = 0;
    let mut pinned = pinned_addresses.iter();
    for i in 0..count {
        let text = format!("item-{i:08}!");
        let expected = string::willow_string_alloc(text.as_ptr(), text.len() as i64);
        let actual = a::willow_array_get(root, i as i64) as *mut u8;
        assert_eq!(string::willow_string_eq(actual, expected), 1);
        if !loan.is_null() {
            assert_eq!(Some(actual as usize), pinned.next().copied());
        }
        checksum += i as u64;
    }
    if shape == "catalog" {
        // Inventory reconciliation: retained product names, bounded stock,
        // deterministic orders, invoice accumulation and final stock queries.
        let mut stocks = a::willow_array_new(count as i64, 0);
        gc::willow_push_root(&mut stocks);
        for i in 0..count {
            a::willow_array_set(stocks, i as i64, 100_000);
        }
        let mut invoice = 0u64;
        let mut expected_invoice = 0u64;
        let mut expected_stocks = vec![100_000i64; count];
        for order in 0..count * 4 {
            let product = (order * 17) % count;
            let quantity = (order % 7 + 1) as i64;
            let stock = a::willow_array_get(stocks, product as i64);
            assert!(stock >= quantity);
            a::willow_array_set(stocks, product as i64, stock - quantity);
            let price = product as u64 % 97 + 1;
            invoice += price * quantity as u64;
            expected_stocks[product] -= quantity;
            expected_invoice += ((order * 17 % count) as u64 % 97 + 1) * (order as u64 % 7 + 1);
            std::hint::black_box(a::willow_array_get(root, product as i64));
            if order % 256 == 0 {
                gc::willow_gc_safepoint();
            }
        }
        gc::willow_gc_collect();
        for (i, stock) in expected_stocks.iter().enumerate() {
            assert_eq!(a::willow_array_get(stocks, i as i64), *stock);
        }
        assert_eq!(invoice, expected_invoice);
        gc::willow_pop_root();
    }
    let after = willow_runtime::gc_telemetry::survivor_snapshot();
    if shape == "young" {
        assert!(
            after.tenured_bytes + after.survivor_bytes
                > before.tenured_bytes + before.survivor_bytes,
            "young copy path not exercised"
        );
    }
    if !loan.is_null() {
        assert!(
            after.pin_reasons[4] > before.pin_reasons[4],
            "scoped young pin path not exercised"
        );
        unsafe { willow_rust_bridge_frame_drop(loan) };
    }
    gc::willow_pop_roots(2);
    checksum
}

fn allocate(count: usize, payload: i64) -> u64 {
    let mut checksum = 0;
    for i in 0..count {
        let before = (i % 64 == 0).then(willow_runtime::gc_telemetry::milestones::snapshot);
        let value = gc::willow_alloc(payload);
        unsafe {
            *value.cast::<u64>() = i as u64;
            checksum += *value.cast::<u64>();
        }
        if let Some(before) = before {
            let after = willow_runtime::gc_telemetry::milestones::snapshot();
            if before.epoch == after.epoch && before.stage == 1 && after.stage == 1 {
                SWEEP_OVERLAP.fetch_add(1, Ordering::Relaxed);
            }
        }
        gc::willow_gc_safepoint();
    }
    checksum
}

pub fn parallel(count: usize, workers: usize) -> u64 {
    parallel_prepared(count, workers, 128, || ()).0
}

fn parallel_prepared<T>(
    count: usize,
    workers: usize,
    payload: i64,
    prepare: impl FnOnce() -> T,
) -> (u64, T) {
    let start = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ready = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let threads: Vec<_> = (1..workers)
        .map(|_| {
            let ready = ready.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                let _registration = gc::MutatorRegistration::new();
                ready.fetch_add(1, Ordering::Release);
                while !start.load(Ordering::Acquire) {
                    gc::willow_gc_safepoint();
                    std::thread::yield_now();
                }
                allocate(count, payload)
            })
        })
        .collect();
    // The root owner is one of the requested mutators, not an extra allocator.
    while ready.load(Ordering::Acquire) < workers {
        gc::willow_gc_safepoint();
        std::thread::yield_now();
    }
    let prepared = prepare();
    start.store(true, Ordering::Release);
    let mut sum = allocate(count, payload);
    while threads.iter().any(|thread| !thread.is_finished()) {
        gc::willow_gc_safepoint();
        std::thread::yield_now();
    }
    sum += threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .sum::<u64>();
    assert_eq!(sum, workers as u64 * count as u64 * (count as u64 - 1) / 2);
    (sum, prepared)
}

pub fn overload(count: usize, workers: usize) -> u64 {
    // Registration during an active epoch may join it. Register every allocator
    // first, then start marking and release the already-enrolled allocators.
    let payload =
        std::env::var("GC_ACCEPTANCE_OVERLOAD_PAYLOAD").map_or(128, |value| value.parse().unwrap());
    let (sum, collector) = parallel_prepared(count, workers, payload, || {
        let prior = willow_runtime::gc_telemetry::milestones::snapshot().epoch;
        let collector = std::thread::spawn(|| {
            let _registration = gc::MutatorRegistration::new();
            // Workers stay held until the requested cycle begins.
            OVERLOAD_PEAK.store(gc::acceptance_collect_scoped(), Ordering::Relaxed);
        });
        while !collector.is_finished()
            && willow_runtime::gc_telemetry::milestones::snapshot().epoch == prior
        {
            gc::willow_gc_safepoint();
            std::thread::yield_now();
        }
        collector
    });
    while !collector.is_finished() {
        gc::willow_gc_safepoint();
        std::thread::yield_now();
    }
    collector.join().unwrap();
    sum
}

pub fn fragmented(count: usize) -> u64 {
    let mut roots = vec![std::ptr::null_mut(); count];
    let mut sizes = vec![0; count];
    for root in &mut roots {
        gc::willow_push_root(root);
    }
    for round in 0..2 {
        for i in 0..count {
            if round == 1 && i % 2 == 0 {
                continue;
            }
            // 256 KiB exceeds the real large-object threshold. Smaller mixed
            // objects leave holes in retained regions after alternating release.
            let size = if i % 31 == 0 {
                262144
            } else {
                4096 + ((i + round) % 7) * 1024
            };
            let object = gc::willow_alloc(size as i64);
            unsafe {
                std::ptr::write_bytes(object, (i % 251) as u8, size);
                *object.cast::<u64>() = i as u64;
            }
            roots[i] = object;
            sizes[i] = size;
        }
        gc::willow_gc_collect();
        if round == 0 {
            for i in (1..count).step_by(2) {
                roots[i] = std::ptr::null_mut();
            }
            gc::willow_gc_collect();
            assert!(
                super::snapshot().baseline.memory.fragmentation_bytes > 0,
                "fragmented retained regions not exercised"
            );
        }
    }
    let mut checksum = 0;
    for (i, (&root, &size)) in roots.iter().zip(&sizes).enumerate() {
        unsafe {
            checksum += *root.cast::<u64>();
            assert!(
                std::slice::from_raw_parts(root.add(8), size - 8)
                    .iter()
                    .all(|&b| b == (i % 251) as u8)
            );
        }
    }
    assert_eq!(checksum, count as u64 * (count as u64 - 1) / 2);
    gc::willow_pop_roots(count as i32);
    checksum
}

/// Registered collector overlaps real minor requests or allocating peers. The
/// trace consumer independently checks the resulting phase overlap.
pub fn overlap(count: usize, workers: usize, young: bool) -> u64 {
    let mut root = std::ptr::null_mut();
    gc::willow_push_root(&mut root);
    if young {
        root = a::willow_array_new(count as i64, 1);
        let mut suffix = string::willow_string_alloc(b"!".as_ptr(), 1);
        gc::willow_push_root(&mut suffix);
        for i in 0..count {
            let text = format!("item-{i:08}");
            let source = string::willow_string_alloc(text.as_ptr(), text.len() as i64);
            let object = string::willow_string_concat(source, suffix);
            a::willow_array_set(root, i as i64, object as i64);
        }
        gc::willow_pop_root();
    }
    let prior = willow_runtime::gc_telemetry::milestones::snapshot().epoch;
    let collector = std::thread::spawn(|| {
        let _registration = gc::MutatorRegistration::new();
        gc::willow_gc_collect();
        gc::willow_gc_safepoint();
    });
    while !collector.is_finished()
        && willow_runtime::gc_telemetry::milestones::snapshot().epoch == prior
    {
        // Generated ABI entry publishes these root slots as relocatable.
        gc::willow_gc_minor_collect();
        std::thread::yield_now();
    }
    let checksum = if young {
        // Request while no runtime value lease is active: overlapping minor
        // deferral is a separate safety path, not completed relocation evidence.
        gc::willow_gc_minor_collect();
        (count as u64) * (count as u64 - 1) / 2
    } else {
        parallel(count, workers)
    };
    if young {
        // Remain in the ABI's relocation-safe wait until the old owner has
        // serviced our minor request. A raw native safepoint would pin/defer it.
        gc::willow_gc_collect();
    }
    while !collector.is_finished() {
        gc::willow_gc_safepoint();
        std::thread::yield_now();
    }
    collector.join().unwrap();
    if young {
        for i in 0..count {
            let text = format!("item-{i:08}!");
            let expected = string::willow_string_alloc(text.as_ptr(), text.len() as i64);
            let actual = a::willow_array_get(root, i as i64) as *mut u8;
            assert_eq!(string::willow_string_eq(actual, expected), 1);
        }
    }
    gc::willow_pop_root();
    checksum
}
