//! Runnable walkthrough of the concurrent-mark work queue (willow-6fv.5.6.1).
//!
//! ```text
//! cargo test -p willow_runtime --lib gc_mark_queue_walkthrough -- --nocapture
//! ```
//!
//! This stands in for the marking engine (willow-6fv.5.6.3): a root job injects
//! shards, background workers consume them and discover more work, a mutator
//! assists, and the cycle ends only once the counters agree that nothing is left
//! anywhere.
//!
//! It lives inside the crate's own test build rather than under
//! `crates/willow_runtime/examples/` because `willow_runtime` exports the `main`
//! symbol that linked Willow programs start from — any separate Rust binary that
//! links the runtime hits a duplicate `main`. It is a `#[test]`, not an ignored
//! demo, so the example cannot rot: the ordinary gate runs it.

use crate::gc_mark_queue::{
    MarkEpoch, MarkWork, MarkWorkItem, MarkWorkQueue, MarkWorker, ObjectRef, RegionId, RootShardId,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const WORKERS: usize = 4;
const ROOT_SHARDS: u32 = 8;
/// Objects each root shard "discovers". Stands in for tracing an object graph.
const OBJECTS_PER_SHARD: usize = 500;

#[test]
fn gc_mark_queue_walkthrough() {
    // One slot per background worker plus one for the assist. Assist claims a
    // free slot when there is one, so a queue sized to the worker count alone
    // would leave a worker slotless whenever the assist registers first.
    let queue = MarkWorkQueue::new(WORKERS + 1);

    // ── initial mark: open a cycle and publish the root set ──────────────────
    let epoch = queue.begin_epoch();
    println!("cycle {} started", epoch.0);

    for shard in 0..ROOT_SHARDS {
        queue
            .inject(MarkWork::new(
                epoch,
                MarkWorkItem::RootShard(RootShardId(shard)),
            ))
            .expect("a current-epoch root shard is always accepted");
    }
    // One oversized region is split into chunks so it does not serialise a whole
    // worker behind a single item.
    for chunk in 0..4 {
        queue
            .inject(MarkWork::new(
                epoch,
                MarkWorkItem::RegionChunk {
                    region: RegionId(1),
                    chunk,
                },
            ))
            .expect("accepted");
    }

    // Work from a cycle that already ended is refused, not delivered: marking it
    // would set bits this cycle's initial mark just cleared.
    let stale = MarkWork::new(
        MarkEpoch(epoch.0.wrapping_sub(1)),
        MarkWorkItem::Object(ObjectRef::from_addr(0xdead_beef).unwrap()),
    );
    let refused = queue
        .inject(stale)
        .expect_err("stale work must never be accepted");
    println!("stale injection refused: {refused:?}");
    println!("published root work: {}", queue.snapshot().published);

    // ── concurrent mark ──────────────────────────────────────────────────────
    let traced = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let expected = ROOT_SHARDS as usize * OBJECTS_PER_SHARD;

    std::thread::scope(|scope| {
        for id in 0..WORKERS {
            let queue = Arc::clone(&queue);
            let traced = Arc::clone(&traced);
            let stop = Arc::clone(&stop);
            scope.spawn(move || {
                let mut worker = queue
                    .register_worker()
                    .expect("the pool is sized to the slot count");
                mark_loop(&mut worker, &traced, &stop);
                println!("worker {id} finished");
            });
        }

        // A mutator that allocated too fast repays its debt by marking. It uses
        // exactly the same consumer interface as a background worker.
        {
            let queue = Arc::clone(&queue);
            let traced = Arc::clone(&traced);
            let stop = Arc::clone(&stop);
            scope.spawn(move || {
                let mut assist = queue.register_assist();
                println!("assist joined (slot: {:?})", assist.slot());
                mark_loop(&mut assist, &traced, &stop);
            });
        }

        // Stand-in for termination detection (willow-6fv.5.6.5): force every
        // private segment into the open so a worker that stalls cannot sit on
        // work its peers could be doing, then read the one counter that knows
        // about every item everywhere — queued or in a consumer's hand.
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            queue.flush_all_locals();
            if queue.snapshot().is_drained() {
                break;
            }
            assert!(Instant::now() < deadline, "marking did not terminate");
            std::thread::yield_now();
        }
        stop.store(true, Ordering::SeqCst);
    });

    // ── cleanup ──────────────────────────────────────────────────────────────
    let snapshot = queue.snapshot();
    println!("objects traced: {}", traced.load(Ordering::SeqCst));
    println!(
        "delivered {} items ({} injected, {} republished, {} steals of {} attempts)",
        snapshot.delivered,
        snapshot.injected,
        snapshot.republished,
        snapshot.steals,
        snapshot.steal_attempts
    );
    println!("stale work rejected: {}", snapshot.stale_rejected);
    assert_eq!(snapshot.counter_underflows, 0);
    assert_eq!(
        snapshot.abandoned, 0,
        "every consumer completed or handed back what it took"
    );
    assert!(
        traced.load(Ordering::SeqCst) >= expected,
        "every discovered object must be traced at least once"
    );

    let leftover = queue.end_epoch();
    assert_eq!(leftover, 0, "a terminated cycle leaves nothing behind");
    println!("cycle {} ended clean", epoch.0);
}

/// A stand-in mark loop: take an item, "trace" it, push what it discovers.
fn mark_loop(worker: &mut MarkWorker, traced: &AtomicUsize, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        let Some(work) = worker.next_work() else {
            std::thread::yield_now();
            continue;
        };
        match work.item {
            MarkWorkItem::RootShard(RootShardId(shard)) => {
                // Scanning a shard discovers objects. They go to the local LIFO,
                // which keeps tracing depth-first; once it grows past the publish
                // threshold the surplus becomes stealable automatically.
                let base = shard as usize * OBJECTS_PER_SHARD + 1;
                for index in 0..OBJECTS_PER_SHARD {
                    let object = ObjectRef::from_addr(base + index).expect("non-zero");
                    worker
                        .push(MarkWorkItem::Object(object))
                        .expect("the cycle is still running");
                }
            }
            MarkWorkItem::Object(_) => {
                traced.fetch_add(1, Ordering::SeqCst);
            }
            MarkWorkItem::ObjectBatch(objects) => {
                traced.fetch_add(objects.len(), Ordering::SeqCst);
            }
            MarkWorkItem::RegionChunk { .. } => {}
        }
    }
}
