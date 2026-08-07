//! Perspectives on the concurrent-mark work queue (willow-6fv.5.6.1).
//!
//! The queue is pure transport, so every test here is about *delivery* and
//! *accounting*, never about what marking does with an item. Item addresses are
//! arbitrary non-zero integers and are never dereferenced.
//!
//! The 41 perspectives below, in order:
//!
//!  1. an idle queue rejects injection — no cycle, no consumers
//!  2. `begin_epoch` advances the epoch and never returns the idle sentinel
//!  3. epoch rollover wraps `u32::MAX -> 1`, skipping idle
//!  4. injected work is published and visible to a consumer
//!  5. stale-epoch injection is rejected and quarantined, never delivered
//!  6. quarantine is capped; overflow is counted and dropped
//!  7. an empty batch is refused and is NOT quarantined
//!  8. local push/pop is LIFO
//!  9. work under the publish threshold stays hidden from peers
//! 10. crossing the publish threshold makes work stealable automatically
//! 11. `publish_local` publishes the OLDEST half, preserving order
//! 12. a steal is bounded by `MAX_STEAL_BATCH`
//! 13. tiny batch: a victim holding one item yields exactly one
//! 14. a steal takes half, leaving the victim work of its own
//! 15. stolen work runs in the victim's original order
//! 16. the injector is FIFO across producers
//! 17. `flush_all_locals` converts every hidden item into published work
//! 18. hidden local work keeps the queue un-drained at termination
//! 19. publication-before-zero: `is_drained` never lies about pending work
//! 20. a consumer counts as active while searching, inactive after failing
//! 21. worker teardown republishes local work and frees the slot
//! 22. `relinquish` republishes and is counted as a licensed duplicate
//! 23. a stale handle stops producing and stops accepting work
//! 24. `end_epoch` discards everything and zeroes the content counters
//! 25. slots are finite; `register_worker` refuses to oversubscribe
//! 26. Assist uses the same consumer interface, slotless when slots run out
//! 27. Assist registrations are counted separately from workers
//! 28. no counter underflows across a full lifecycle
//! 29. safepoint discipline: the lock-held flag is set only inside a critical
//!     section, and `assert_no_queue_lock_held` fires when it is set
//! 30. 1/5/32-worker correctness stress: every item delivered at least once
//! 31. producer/consumer race: concurrent injection loses nothing
//! 32. weak-memory stress: hammered publish/drain keeps the counter contract
//! 33. nightly-only high-volume stress (ignored by default)
//! 34. lock order: a flush racing owners that refill from their own shared
//!     segment must not deadlock
//! 35. an epoch number is never issued twice, not even across `end_epoch`
//! 36. a handle from before an `end_epoch`/`begin_epoch` pair stays stale
//! 37. `inject` racing `begin_epoch` on a barrier never leaks stale work
//! 38. `is_drained` is never true while any item is unconsumed, under load
//! 39. dropping a consumer with an item in hand is counted, not silent
//! 40. relinquishing work that was never handed out counts as a new insertion
//! 41. `is_drained` reads exactly one counter, so it cannot be torn

use super::*;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

fn object(addr: usize) -> MarkWorkItem {
    MarkWorkItem::Object(ObjectRef::from_addr(addr).expect("non-zero test address"))
}

fn object_addr(work: &MarkWork) -> usize {
    match &work.item {
        MarkWorkItem::Object(object) => object.addr(),
        other => panic!("expected a single-object item, got {other:?}"),
    }
}

fn started_queue(slots: usize) -> (Arc<MarkWorkQueue>, MarkEpoch) {
    let queue = MarkWorkQueue::new(slots);
    let epoch = queue.begin_epoch();
    (queue, epoch)
}

/// Drain a consumer completely, returning the addresses in delivery order.
fn drain(worker: &mut MarkWorker) -> Vec<usize> {
    let mut seen = Vec::new();
    while let Some(work) = worker.next_work() {
        seen.push(object_addr(&work));
    }
    seen
}

/// Wait for `condition`, failing loudly instead of hanging CI.
fn wait_for(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::yield_now();
    }
}

// 1. Injection into a queue with no running cycle is refused: there is no epoch
//    to tag the work with and nobody to consume it.
#[test]
fn mark_queue_01_idle_queue_rejects_injection() {
    let queue = MarkWorkQueue::new(2);
    assert!(queue.current_epoch().is_idle());

    let rejected = queue.inject(MarkWork::new(MarkEpoch(1), object(0x10)));
    assert_eq!(rejected, Err(RejectReason::Idle));

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.published, 0);
    assert_eq!(snapshot.injected, 0);
    assert_eq!(snapshot.stale_rejected, 1);
    assert!(snapshot.is_drained());
}

// 2. Each cycle gets a fresh epoch, and the idle sentinel is never handed out as
//    a live epoch.
#[test]
fn mark_queue_02_begin_epoch_advances_and_never_returns_idle() {
    let queue = MarkWorkQueue::new(1);
    let first = queue.begin_epoch();
    let second = queue.begin_epoch();
    let third = queue.begin_epoch();

    assert!(!first.is_idle() && !second.is_idle() && !third.is_idle());
    assert_ne!(first, second);
    assert_ne!(second, third);
    assert_eq!(queue.current_epoch(), third);

    assert_eq!(queue.end_epoch(), 0);
    assert!(queue.current_epoch().is_idle());
}

// 3. A long-lived process rolls the epoch counter over. It must wrap to 1, not
//    to 0, because 0 means "no cycle running" and would make every live item
//    look stale.
#[test]
fn mark_queue_03_epoch_rollover_skips_the_idle_sentinel() {
    assert_eq!(MarkEpoch(u32::MAX).next(), MarkEpoch(1));
    assert_eq!(MarkEpoch::IDLE.next(), MarkEpoch(1));
    assert_eq!(MarkEpoch(7).next(), MarkEpoch(8));

    let queue = MarkWorkQueue::new(1);
    // The issuing counter is what rolls over, not the running epoch: ending a
    // cycle parks `epoch` at idle and leaves `last_issued` where it was.
    queue.last_issued.store(u32::MAX, Ordering::SeqCst);
    queue.epoch.store(u32::MAX, Ordering::SeqCst);
    let rolled = queue.begin_epoch();
    assert_eq!(rolled, MarkEpoch(1));

    // Work tagged with the pre-rollover epoch is stale. Staleness is decided by
    // inequality, never by ordering, so a smaller epoch number after a wrap is
    // still "current".
    assert_eq!(
        queue.inject(MarkWork::new(MarkEpoch(u32::MAX), object(0x20))),
        Err(RejectReason::StaleEpoch)
    );
    assert!(queue.inject(MarkWork::new(rolled, object(0x21))).is_ok());
}

// 4. The basic path: an external producer injects, a consumer receives.
#[test]
fn mark_queue_04_injected_work_reaches_a_consumer() {
    let (queue, epoch) = started_queue(1);
    queue
        .inject(MarkWork::new(epoch, object(0x30)))
        .expect("current-epoch injection is accepted");

    let before = queue.snapshot();
    assert_eq!(before.published, 1);
    assert_eq!(before.injected, 1);
    assert!(!before.is_drained());

    let mut worker = queue.register_worker().expect("a free slot");
    assert_eq!(drain(&mut worker), vec![0x30]);

    let after = queue.snapshot();
    assert_eq!(after.published, 0);
    assert_eq!(after.hidden, 0);
    assert_eq!(after.delivered, 1);
    assert_eq!(after.active_consumers, 0);
    assert!(after.is_drained());
}

// 5. A producer preempted across an epoch boundary hands over work for a cycle
//    that is over. It must never be delivered — marking it would set mark bits
//    that the new cycle's initial mark just cleared.
#[test]
fn mark_queue_05_stale_epoch_injection_is_quarantined_not_delivered() {
    let (queue, first) = started_queue(1);
    let second = queue.begin_epoch();
    assert_ne!(first, second);

    assert_eq!(
        queue.inject(MarkWork::new(first, object(0x40))),
        Err(RejectReason::StaleEpoch)
    );

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.published, 0);
    assert_eq!(snapshot.stale_rejected, 1);
    assert_eq!(snapshot.quarantined, 1);

    let mut worker = queue.register_worker().expect("a free slot");
    assert!(worker.next_work().is_none());

    let quarantined = queue.take_quarantine();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0].epoch, first);
    assert_eq!(queue.snapshot().quarantined, 0);
}

// 6. Quarantine is a diagnostic buffer, not a second queue: a storm of stale
//    work must not grow memory without bound. Everything is still counted.
#[test]
fn mark_queue_06_quarantine_is_capped_and_overflow_is_counted() {
    let (queue, first) = started_queue(1);
    queue.begin_epoch();

    let storm = MAX_QUARANTINED_ITEMS * 3;
    for index in 0..storm {
        assert_eq!(
            queue.inject(MarkWork::new(first, object(0x1000 + index))),
            Err(RejectReason::StaleEpoch)
        );
    }

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.stale_rejected, storm as u64);
    assert_eq!(snapshot.quarantined, MAX_QUARANTINED_ITEMS as u64);
    assert_eq!(queue.take_quarantine().len(), MAX_QUARANTINED_ITEMS);
    assert_eq!(queue.snapshot().quarantined, 0);
    assert_eq!(queue.snapshot().counter_underflows, 0);
}

// 7. An empty batch would occupy a queue slot and a termination check while
//    marking nothing. It is refused — and, unlike stale work, it is not worth
//    quarantining, because there is nothing to inspect.
#[test]
fn mark_queue_07_empty_batches_are_refused_without_quarantine() {
    let (queue, epoch) = started_queue(1);
    let empty = MarkWorkItem::ObjectBatch(Vec::new().into_boxed_slice());
    assert!(empty.is_empty());
    assert_eq!(empty.object_count(), 0);

    assert_eq!(
        queue.inject(MarkWork::new(epoch, empty)),
        Err(RejectReason::Empty)
    );

    let mut worker = queue.register_worker().expect("a free slot");
    assert_eq!(
        worker.push(MarkWorkItem::ObjectBatch(Vec::new().into_boxed_slice())),
        Err(RejectReason::Empty)
    );

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.published, 0);
    assert_eq!(snapshot.hidden, 0);
    assert_eq!(snapshot.quarantined, 0);
    assert_eq!(snapshot.stale_rejected, 0);
}

// 8. The owner's own segment is LIFO, which is what keeps tracing depth-first
//    and the object graph cache-warm.
#[test]
fn mark_queue_08_local_push_pop_is_lifo() {
    let (queue, _) = started_queue(1);
    let mut worker = queue.register_worker().expect("a free slot");

    for addr in [0x51, 0x52, 0x53] {
        worker.push(object(addr)).expect("push accepted");
    }
    assert_eq!(drain(&mut worker), vec![0x53, 0x52, 0x51]);
}

// 9. Work below the publish threshold is HIDDEN: counted, but deliberately not
//    stealable, so the owner keeps its depth-first locality.
#[test]
fn mark_queue_09_work_below_the_threshold_stays_hidden() {
    let (queue, _) = started_queue(2);
    let mut owner = queue.register_worker().expect("slot 0");
    let mut thief = queue.register_worker().expect("slot 1");

    for addr in 1..=4 {
        owner.push(object(0x60 + addr)).expect("push accepted");
    }

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.hidden, 4);
    assert_eq!(snapshot.published, 0);
    assert_eq!(snapshot.outstanding, 4);
    assert!(!snapshot.is_drained(), "hidden work is still work");

    assert!(
        thief.next_work().is_none(),
        "a private segment must not be stealable"
    );
}

// 10. A worker that falls behind becomes a donor without being told to: once its
//     private segment passes the threshold, half of it becomes stealable.
#[test]
fn mark_queue_10_crossing_the_threshold_publishes_automatically() {
    let (queue, _) = started_queue(2);
    let mut owner = queue.register_worker().expect("slot 0");
    let mut thief = queue.register_worker().expect("slot 1");

    for index in 0..=LOCAL_PUBLISH_THRESHOLD {
        owner.push(object(0x2000 + index)).expect("push accepted");
    }

    let snapshot = queue.snapshot();
    assert!(
        snapshot.published > 0,
        "crossing the threshold must publish: {snapshot:?}"
    );
    assert_eq!(snapshot.outstanding, LOCAL_PUBLISH_THRESHOLD as u64 + 1);

    let stolen = thief.next_work().expect("published work is stealable");
    assert_eq!(queue.snapshot().steals, 1);
    // The thief took the OLDEST published item, the one the owner would reach
    // last.
    assert_eq!(object_addr(&stolen), 0x2000);
}

// 11. `publish_local` gives away the oldest half. Oldest first is deliberate: a
//     LIFO owner wants its newest items, and the oldest are the coldest.
#[test]
fn mark_queue_11_publish_local_donates_the_oldest_half_in_order() {
    let (queue, _) = started_queue(2);
    let mut owner = queue.register_worker().expect("slot 0");
    let mut thief = queue.register_worker().expect("slot 1");

    for index in 0..6 {
        owner.push(object(0x70 + index)).expect("push accepted");
    }
    assert_eq!(owner.publish_local(usize::MAX), 3);

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.published, 3);
    assert_eq!(snapshot.hidden, 3);

    // The thief drains the donated half; the owner keeps the newest three and
    // still runs them LIFO.
    assert_eq!(drain(&mut thief), vec![0x70, 0x71, 0x72]);
    assert_eq!(drain(&mut owner), vec![0x75, 0x74, 0x73]);
}

// 12. A steal is a bounded batch. Unbounded steals would just move the imbalance
//     and would hold the victim's lock for an unbounded time.
#[test]
fn mark_queue_12_steals_are_bounded_by_the_batch_limit() {
    let (queue, _) = started_queue(2);
    let mut owner = queue.register_worker().expect("slot 0");
    let mut thief = queue.register_worker().expect("slot 1");

    let total = MAX_STEAL_BATCH * 8;
    for index in 0..total {
        owner.push(object(0x3000 + index)).expect("push accepted");
    }
    owner.publish_local(usize::MAX);
    let published_before = queue.snapshot().published;
    assert!(published_before > MAX_STEAL_BATCH as u64);

    thief.next_work().expect("a stealable item");
    let moved = published_before - queue.snapshot().published;
    assert!(
        moved <= MAX_STEAL_BATCH as u64,
        "one steal moved {moved} items, limit is {MAX_STEAL_BATCH}"
    );
}

// 13. The degenerate batch: a victim holding exactly one item must still yield
//     it. A "half the queue" rule that rounds to zero would strand it.
#[test]
fn mark_queue_13_tiny_batches_still_transfer() {
    let (queue, _) = started_queue(2);
    let mut owner = queue.register_worker().expect("slot 0");
    let mut thief = queue.register_worker().expect("slot 1");

    owner.push(object(0x80)).expect("push accepted");
    assert_eq!(owner.publish_local(usize::MAX), 1);

    let stolen = thief.next_work().expect("the single item is stealable");
    assert_eq!(object_addr(&stolen), 0x80);
    assert!(queue.snapshot().is_drained() || queue.snapshot().active_consumers > 0);
}

// 14. A steal must not strip the victim bare, or two idle consumers ping-pong
//     the same items instead of both making progress.
#[test]
fn mark_queue_14_a_steal_leaves_the_victim_some_work() {
    let (queue, _) = started_queue(2);
    let mut owner = queue.register_worker().expect("slot 0");
    let mut thief = queue.register_worker().expect("slot 1");

    for index in 0..8 {
        owner.push(object(0x90 + index)).expect("push accepted");
    }
    owner.publish_local(usize::MAX); // 4 published, 4 hidden

    thief.next_work().expect("a stealable item");
    let remaining = queue.snapshot().published;
    assert!(
        remaining > 0,
        "the thief emptied the victim's shared segment"
    );
}

// 15. Stolen work runs in the order the victim would have run it: the thief
//     pushes the batch reversed into its LIFO so the oldest comes out first.
#[test]
fn mark_queue_15_stolen_work_keeps_the_victims_order() {
    let (queue, _) = started_queue(2);
    let mut owner = queue.register_worker().expect("slot 0");
    let mut thief = queue.register_worker().expect("slot 1");

    for index in 0..4 {
        owner.push(object(0xA0 + index)).expect("push accepted");
    }
    owner.publish_local(usize::MAX);
    assert_eq!(drain(&mut thief), vec![0xA0, 0xA1]);
}

// 16. External producers publish through the injector, and the injector is FIFO:
//     root shards queued first are scanned first, so root work does not starve
//     behind later SATB flushes.
#[test]
fn mark_queue_16_the_injector_is_fifo() {
    let (queue, epoch) = started_queue(1);
    for index in 0..5 {
        queue
            .inject(MarkWork::new(epoch, object(0xB0 + index)))
            .expect("accepted");
    }

    let mut worker = queue.register_worker().expect("a free slot");
    assert_eq!(drain(&mut worker), vec![0xB0, 0xB1, 0xB2, 0xB3, 0xB4]);
}

// 17. Termination has to be able to force every hidden item into the open.
#[test]
fn mark_queue_17_flush_all_locals_publishes_every_hidden_item() {
    let (queue, _) = started_queue(3);
    let mut first = queue.register_worker().expect("slot 0");
    let mut second = queue.register_worker().expect("slot 1");

    for index in 0..3 {
        first.push(object(0xC0 + index)).expect("push accepted");
        second.push(object(0xD0 + index)).expect("push accepted");
    }
    assert_eq!(queue.snapshot().hidden, 6);

    assert_eq!(queue.flush_all_locals(), 6);
    let snapshot = queue.snapshot();
    assert_eq!(snapshot.hidden, 0, "nothing may stay hidden after a flush");
    assert_eq!(snapshot.published, 6);

    // A third consumer that owns none of that work can now reach all of it.
    let mut late = queue.register_worker().expect("slot 2");
    let mut seen = drain(&mut late);
    seen.sort_unstable();
    assert_eq!(seen, vec![0xC0, 0xC1, 0xC2, 0xD0, 0xD1, 0xD2]);
}

// 18. The hidden-work trap: the injector is empty and no consumer is active, yet
//     marking is NOT finished. A termination check that only looked at the
//     injector would end the cycle with live objects unmarked.
#[test]
fn mark_queue_18_hidden_local_work_keeps_the_queue_undrained() {
    let (queue, _) = started_queue(2);
    let mut owner = queue.register_worker().expect("slot 0");
    owner.push(object(0xE0)).expect("push accepted");
    // The owner goes idle without consuming its own push.
    assert_eq!(queue.snapshot().active_consumers, 0);

    let injector_empty = QueueGuard::new(&queue.injector).is_empty();
    assert!(injector_empty, "the item never reached the injector");

    let snapshot = queue.snapshot();
    assert!(
        !snapshot.is_drained(),
        "hidden work must defeat the termination predicate: {snapshot:?}"
    );
    assert_eq!(snapshot.outstanding, 1);

    assert_eq!(queue.flush_all_locals(), 1);
    assert!(!queue.snapshot().is_drained());
    let mut other = queue.register_worker().expect("slot 1");
    assert_eq!(drain(&mut other), vec![0xE0]);
    assert!(queue.snapshot().is_drained());
}

// 19. The publication-before-zero rule, checked step by step: from injection to
//     delivery there is no instant at which the queue reports itself drained.
//     Everything below is a single thread, so each assertion is a real instant.
#[test]
fn mark_queue_19_is_drained_never_lies_about_pending_work() {
    let (queue, epoch) = started_queue(2);
    assert!(queue.snapshot().is_drained(), "an empty queue is drained");

    queue
        .inject(MarkWork::new(epoch, object(0xF0)))
        .expect("accepted");
    assert!(!queue.snapshot().is_drained(), "published work pends");

    let mut worker = queue.register_worker().expect("slot 0");
    let work = worker.next_work().expect("the injected item");
    assert!(
        !queue.snapshot().is_drained(),
        "the item is in a consumer's hand: active_consumers must cover it"
    );
    assert_eq!(queue.snapshot().active_consumers, 1);

    // Work discovered while marking moves straight into the hidden segment; the
    // predicate has to keep covering it there.
    worker.push(object(0xF1)).expect("push accepted");
    assert!(!queue.snapshot().is_drained());
    drop(work);

    assert_eq!(drain(&mut worker), vec![0xF1]);
    assert!(queue.snapshot().is_drained(), "everything really is done");
}

// 20. A consumer is active from the moment it starts searching, not from the
//     moment it succeeds. A worker that is between items must not read as idle.
#[test]
fn mark_queue_20_consumers_are_active_while_searching() {
    let (queue, epoch) = started_queue(1);
    let mut worker = queue.register_worker().expect("slot 0");
    assert_eq!(queue.snapshot().active_consumers, 0);

    queue
        .inject(MarkWork::new(epoch, object(0x100)))
        .expect("accepted");
    worker.next_work().expect("the injected item");
    assert_eq!(queue.snapshot().active_consumers, 1);

    // Still active across a successful second search.
    worker.push(object(0x101)).expect("push accepted");
    worker.next_work().expect("the pushed item");
    assert_eq!(queue.snapshot().active_consumers, 1);

    assert!(worker.next_work().is_none());
    assert_eq!(
        queue.snapshot().active_consumers,
        0,
        "a failed search deactivates"
    );
}

// 21. A worker that exits with local work must not take it to the grave. Its
//     slot must also come back, or a restarted worker pool would run short.
#[test]
fn mark_queue_21_worker_teardown_republishes_local_work_and_frees_the_slot() {
    let (queue, _) = started_queue(1);
    {
        let mut worker = queue.register_worker().expect("slot 0");
        assert_eq!(worker.slot(), Some(0));
        for index in 0..3 {
            worker.push(object(0x110 + index)).expect("push accepted");
        }
        assert_eq!(queue.snapshot().registered_workers, 1);
        assert!(
            queue.register_worker().is_none(),
            "the only slot is taken while the worker lives"
        );
    }

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.hidden, 0, "teardown must publish hidden work");
    assert_eq!(snapshot.published, 3);
    assert_eq!(snapshot.registered_workers, 0);
    assert_eq!(snapshot.active_consumers, 0);

    let mut replacement = queue.register_worker().expect("the slot came back");
    let mut seen = drain(&mut replacement);
    seen.sort_unstable();
    assert_eq!(seen, vec![0x110, 0x111, 0x112]);
}

// 22. Handing back an unfinished item is the ONE place duplicates come from, so
//     it is counted: the at-least-once contract has a measurable price.
#[test]
fn mark_queue_22_relinquish_republishes_and_counts_the_duplicate() {
    let (queue, epoch) = started_queue(2);
    queue
        .inject(MarkWork::new(epoch, object(0x120)))
        .expect("accepted");

    let mut first = queue.register_worker().expect("slot 0");
    let work = first.next_work().expect("the injected item");

    // In hand, and therefore still outstanding: the item sits in no segment at
    // all, so the advisory content counters read zero, and only `outstanding`
    // keeps the cycle from being declared finished with this object unmarked.
    let held = queue.snapshot();
    assert_eq!(held.delivered, 1);
    assert_eq!(held.published, 0);
    assert_eq!(held.hidden, 0);
    assert_eq!(held.outstanding, 1);
    assert!(!held.is_drained(), "an item in a consumer's hand is work");

    // Handing it back moves it from the hand to the injector. It was counted in
    // both places, so the total does not move.
    first.relinquish(work);
    let snapshot = queue.snapshot();
    assert_eq!(snapshot.republished, 1);
    assert_eq!(snapshot.published, 1);
    assert_eq!(snapshot.outstanding, 1);

    let mut second = queue.register_worker().expect("slot 1");
    assert_eq!(drain(&mut second), vec![0x120]);
    let snapshot = queue.snapshot();
    assert_eq!(
        snapshot.delivered, 2,
        "the same item was delivered twice, by design"
    );
    // `first` gave the item back but never searched again, so it is still
    // counted active. That no longer holds up termination: a consumer with
    // nothing in hand cannot produce work, so `active_consumers` is diagnostic
    // and `outstanding` alone decides. `second` completed the item when its
    // final `next_work` came up empty.
    assert_eq!(snapshot.active_consumers, 1);
    assert_eq!(snapshot.outstanding, 0);
    assert!(snapshot.is_drained());
    drop(first);
    assert!(queue.snapshot().is_drained());
    assert_eq!(queue.snapshot().abandoned, 0, "nothing was dropped in hand");
}

// 23. A handle that outlives its cycle is inert: it produces nothing and accepts
//     nothing, rather than injecting last cycle's work into this one.
#[test]
fn mark_queue_23_a_stale_handle_stops_producing_and_consuming() {
    let (queue, _) = started_queue(1);
    let mut worker = queue.register_worker().expect("slot 0");
    worker.push(object(0x130)).expect("push accepted");
    assert!(!worker.is_stale());

    queue.begin_epoch();
    assert!(worker.is_stale());
    assert_eq!(worker.push(object(0x131)), Err(RejectReason::StaleEpoch));
    assert!(worker.next_work().is_none());

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.published, 0, "begin_epoch discarded the old cycle");
    assert_eq!(snapshot.hidden, 0);
    assert_eq!(snapshot.active_consumers, 0);
    assert_eq!(snapshot.counter_underflows, 0);
}

// 24. Ending a cycle drops everything still queued and reports how much. A
//     terminated cycle should report zero; anything else means termination
//     detection concluded early.
#[test]
fn mark_queue_24_end_epoch_discards_everything_without_underflow() {
    let (queue, epoch) = started_queue(2);
    let mut worker = queue.register_worker().expect("slot 0");
    for index in 0..4 {
        queue
            .inject(MarkWork::new(epoch, object(0x140 + index)))
            .expect("accepted");
        worker.push(object(0x150 + index)).expect("push accepted");
    }
    worker.publish_local(usize::MAX);

    let discarded = queue.end_epoch();
    assert_eq!(discarded, 8);
    assert!(queue.current_epoch().is_idle());

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.published, 0);
    assert_eq!(snapshot.hidden, 0);
    assert_eq!(snapshot.counter_underflows, 0);

    // The now-stale worker deactivating must not push a counter below zero.
    assert!(worker.next_work().is_none());
    drop(worker);
    assert_eq!(queue.snapshot().counter_underflows, 0);
    assert_eq!(queue.snapshot().registered_workers, 0);
}

// 25. Slots are a fixed resource sized to the worker pool. Oversubscription is
//     the caller's bug, and it is reported rather than silently degrading.
#[test]
fn mark_queue_25_slots_are_finite() {
    let (queue, _) = started_queue(2);
    assert_eq!(queue.slot_count(), 2);
    let first = queue.register_worker().expect("slot 0");
    let second = queue.register_worker().expect("slot 1");
    assert!(queue.register_worker().is_none());
    assert_ne!(first.slot(), second.slot());

    drop(first);
    let third = queue.register_worker().expect("a slot was released");
    assert_eq!(third.slot(), Some(0));
}

// 26. Mutator Assist gets the SAME consumer interface as a mark worker — the
//     same type and the same methods — so assist can never drift from worker
//     behaviour. Assist is unbounded in number, so when slots run out it runs
//     slotless: it still consumes, and its discoveries go to the injector.
#[test]
fn mark_queue_26_assist_shares_the_consumer_interface() {
    let (queue, epoch) = started_queue(1);
    let _worker = queue.register_worker().expect("the only slot");

    let mut assist = queue.register_assist();
    assert!(assist.is_assist());
    assert_eq!(assist.slot(), None, "no slot was free");

    queue
        .inject(MarkWork::new(epoch, object(0x160)))
        .expect("accepted");
    let work = assist.next_work().expect("assist consumes like a worker");
    assert_eq!(object_addr(&work), 0x160);

    // A slotless assist has nowhere to hide work, so its pushes are published
    // immediately.
    assist.push(object(0x161)).expect("push accepted");
    let snapshot = queue.snapshot();
    assert_eq!(snapshot.hidden, 0);
    assert_eq!(snapshot.published, 1);

    assert_eq!(assist.publish_local(usize::MAX), 0);
    assert_eq!(drain(&mut assist), vec![0x161]);
}

// 27. Assist and worker registrations are counted apart: the Pacer needs to know
//     how much of the marking throughput is being paid for by mutators.
#[test]
fn mark_queue_27_assist_registrations_are_counted_separately() {
    let (queue, _) = started_queue(4);
    let worker = queue.register_worker().expect("slot 0");
    let assist = queue.register_assist();
    assert_eq!(
        assist.slot(),
        Some(1),
        "assist takes a slot when one is free"
    );

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.registered_workers, 1);
    assert_eq!(snapshot.registered_assists, 1);

    drop(assist);
    assert_eq!(queue.snapshot().registered_assists, 0);
    assert_eq!(queue.snapshot().registered_workers, 1);
    drop(worker);
    assert_eq!(queue.snapshot().registered_workers, 0);
    assert_eq!(queue.snapshot().counter_underflows, 0);
}

// 28. Counters saturate at zero and record the fact. A wrapped counter would
//     read as `u64::MAX` outstanding work and hang termination forever, so this
//     sweeps a whole lifecycle and insists nothing ever went negative.
#[test]
fn mark_queue_28_no_counter_underflows_across_a_lifecycle() {
    let queue = MarkWorkQueue::new(3);
    for cycle in 0..4 {
        let epoch = queue.begin_epoch();
        let mut worker = queue.register_worker().expect("a free slot");
        let mut assist = queue.register_assist();

        for index in 0..40 {
            queue
                .inject(MarkWork::new(epoch, object(0x200 + cycle * 100 + index)))
                .expect("accepted");
            worker.push(object(0x300 + cycle * 100 + index)).ok();
        }
        worker.publish_local(usize::MAX);
        queue.flush_all_locals();
        drain(&mut assist);
        drain(&mut worker);

        if cycle % 2 == 0 {
            queue.end_epoch();
        }
        assert_eq!(queue.snapshot().counter_underflows, 0, "cycle {cycle}");
    }
    queue.end_epoch();

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.counter_underflows, 0);
    assert_eq!(snapshot.published, 0);
    assert_eq!(snapshot.hidden, 0);
    assert_eq!(snapshot.active_consumers, 0);
}

// 29. Blocking at a safepoint while holding a queue lock deadlocks the
//     collector: the world cannot restart until the lock holder runs, and the
//     lock holder is waiting for the world to restart. The rule is enforced,
//     not just documented.
#[test]
fn mark_queue_29_safepoint_discipline_is_enforceable() {
    let (queue, _) = started_queue(1);
    assert!(!queue_lock_held());
    assert_no_queue_lock_held("outside any critical section");

    {
        let _guard = QueueGuard::new(&queue.injector);
        assert!(queue_lock_held(), "the guard must record ownership");
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_no_queue_lock_held("inside a critical section");
        }));
        assert!(caught.is_err(), "the assertion must fire while locked");
    }

    assert!(!queue_lock_held(), "the guard must clear it on drop");
    assert_no_queue_lock_held("after the critical section");

    // Every public operation must leave the flag clear, including ones that take
    // two locks.
    let mut worker = queue.register_worker().expect("slot 0");
    worker.push(object(0x170)).expect("push accepted");
    worker.publish_local(usize::MAX);
    queue.flush_all_locals();
    worker.next_work();
    queue.snapshot();
    assert!(!queue_lock_held());
}

// 30. The headline correctness property, at 1, 5, and 32 consumers: every
//     published item is delivered at least once and none is lost. 32 workers
//     with 4 slots' worth of contention is where a lost item would show up.
#[test]
fn mark_queue_30_multi_worker_delivery_is_lossless() {
    for workers in [1usize, 5, 32] {
        let queue = MarkWorkQueue::new(workers);
        let epoch = queue.begin_epoch();
        let total = 4000usize;

        for index in 0..total {
            queue
                .inject(MarkWork::new(epoch, object(index + 1)))
                .expect("accepted");
        }

        let delivered = Arc::new(Mutex::new(Vec::<usize>::new()));
        let seen_count = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        std::thread::scope(|scope| {
            for _ in 0..workers {
                let queue = Arc::clone(&queue);
                let delivered = Arc::clone(&delivered);
                let seen_count = Arc::clone(&seen_count);
                let stop = Arc::clone(&stop);
                scope.spawn(move || {
                    let mut worker = queue.register_worker().expect("one slot per worker");
                    let mut local = Vec::new();
                    while !stop.load(Ordering::SeqCst) {
                        match worker.next_work() {
                            Some(work) => {
                                local.push(object_addr(&work));
                                seen_count.fetch_add(1, Ordering::SeqCst);
                            }
                            None => std::thread::yield_now(),
                        }
                    }
                    delivered.lock().unwrap().extend(local);
                });
            }

            wait_for(
                &format!("{workers} workers to deliver {total} items"),
                Duration::from_secs(60),
                || seen_count.load(Ordering::SeqCst) >= total,
            );
            stop.store(true, Ordering::SeqCst);
        });

        let delivered = delivered.lock().unwrap();
        let unique: HashSet<usize> = delivered.iter().copied().collect();
        assert_eq!(
            unique.len(),
            total,
            "{workers} workers lost items: got {} unique of {total}",
            unique.len()
        );
        assert_eq!(*unique.iter().min().unwrap(), 1);
        assert_eq!(*unique.iter().max().unwrap(), total);

        let snapshot = queue.snapshot();
        assert!(snapshot.is_drained(), "{workers} workers: {snapshot:?}");
        assert_eq!(snapshot.counter_underflows, 0);
    }
}

// 31. Producers and consumers running at the same time. Injection racing against
//     stealing is where a published-but-invisible item would hide, and where a
//     counter decremented before the item was removed would show up as an early
//     drained reading.
#[test]
fn mark_queue_31_producers_and_consumers_race_without_loss() {
    let queue = MarkWorkQueue::new(4);
    let epoch = queue.begin_epoch();
    let producers = 4usize;
    let per_producer = 2000usize;
    let total = producers * per_producer;

    let delivered = Arc::new(Mutex::new(Vec::<usize>::new()));
    let seen_count = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    std::thread::scope(|scope| {
        for producer in 0..producers {
            let queue = Arc::clone(&queue);
            scope.spawn(move || {
                for index in 0..per_producer {
                    let addr = producer * per_producer + index + 1;
                    queue
                        .inject(MarkWork::new(epoch, object(addr)))
                        .expect("accepted");
                    if index % 128 == 0 {
                        std::thread::yield_now();
                    }
                }
            });
        }

        for _ in 0..4 {
            let queue = Arc::clone(&queue);
            let delivered = Arc::clone(&delivered);
            let seen_count = Arc::clone(&seen_count);
            let stop = Arc::clone(&stop);
            scope.spawn(move || {
                let mut worker = queue.register_worker().expect("one slot per worker");
                let mut local = Vec::new();
                while !stop.load(Ordering::SeqCst) {
                    match worker.next_work() {
                        Some(work) => {
                            local.push(object_addr(&work));
                            seen_count.fetch_add(1, Ordering::SeqCst);
                        }
                        None => std::thread::yield_now(),
                    }
                }
                delivered.lock().unwrap().extend(local);
            });
        }

        wait_for(
            "a racing producer/consumer round to finish",
            Duration::from_secs(60),
            || seen_count.load(Ordering::SeqCst) >= total,
        );
        stop.store(true, Ordering::SeqCst);
    });

    let delivered = delivered.lock().unwrap();
    let unique: HashSet<usize> = delivered.iter().copied().collect();
    assert_eq!(unique.len(), total, "items were lost under a producer race");

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.injected, total as u64);
    assert!(
        snapshot.delivered >= snapshot.injected,
        "at-least-once delivery: {snapshot:?}"
    );
    assert_eq!(snapshot.counter_underflows, 0);
}

// 32. Weak-memory stress on the ordering contract itself.
//
//     The observer reads `injected` FIRST and the emptiness counters AFTER. If
//     the queue then reports drained, every one of those `injected` items had
//     already left the queue and no consumer held one, so `delivered` must have
//     caught up. Reading in that order is what makes the claim sound on a weakly
//     ordered machine; SeqCst on every counter is what makes the order real.
#[test]
fn mark_queue_32_publication_before_zero_holds_under_stress() {
    for round in 0..8 {
        let queue = MarkWorkQueue::new(2);
        let epoch = queue.begin_epoch();
        let total = 3000usize;
        let seen_count = Arc::new(AtomicUsize::new(0));
        let observations = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|scope| {
            {
                let queue = Arc::clone(&queue);
                scope.spawn(move || {
                    for index in 0..total {
                        queue
                            .inject(MarkWork::new(epoch, object(index + 1)))
                            .expect("accepted");
                    }
                });
            }
            {
                let queue = Arc::clone(&queue);
                let seen_count = Arc::clone(&seen_count);
                scope.spawn(move || {
                    let mut worker = queue.register_worker().expect("slot 0");
                    while seen_count.load(Ordering::SeqCst) < total {
                        if worker.next_work().is_some() {
                            seen_count.fetch_add(1, Ordering::SeqCst);
                        } else {
                            std::thread::yield_now();
                        }
                    }
                });
            }
            {
                let queue = Arc::clone(&queue);
                let seen_count = Arc::clone(&seen_count);
                let observations = Arc::clone(&observations);
                scope.spawn(move || {
                    let counters = &queue.counters;
                    while seen_count.load(Ordering::SeqCst) < total {
                        let injected = counters.injected.load(Ordering::SeqCst);
                        let published = counters.published.load(Ordering::SeqCst);
                        let hidden = counters.hidden.load(Ordering::SeqCst);
                        let active = counters.active_consumers.load(Ordering::SeqCst);
                        if published == 0 && hidden == 0 && active == 0 {
                            let delivered = counters.delivered.load(Ordering::SeqCst);
                            assert!(
                                delivered >= injected,
                                "round {round}: queue reported drained with \
                                 {injected} injected but only {delivered} delivered"
                            );
                            observations.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                });
            }
        });

        assert_eq!(seen_count.load(Ordering::SeqCst), total);
        assert_eq!(queue.snapshot().counter_underflows, 0);
    }
}

// 33. High-volume soak. Deliberately NOT part of the ordinary suite: it takes
//     minutes and allocates heavily, so it belongs to a nightly target.
//
//         cargo test -p willow_runtime --release -- --ignored mark_queue_33
#[test]
#[ignore = "nightly stress: ~100M items, minutes of runtime"]
fn mark_queue_33_nightly_high_volume_stress() {
    let workers = 8usize;
    let queue = MarkWorkQueue::new(workers);
    let epoch = queue.begin_epoch();
    let total: usize = 100_000_000;
    let per_producer = total / 4;

    let seen_count = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    std::thread::scope(|scope| {
        for producer in 0..4 {
            let queue = Arc::clone(&queue);
            scope.spawn(move || {
                for index in 0..per_producer {
                    let addr = producer * per_producer + index + 1;
                    // Back off rather than let the injector grow without bound:
                    // the point of the soak is sustained throughput, not memory.
                    while queue.snapshot().outstanding > 1_000_000 {
                        std::thread::yield_now();
                    }
                    queue
                        .inject(MarkWork::new(epoch, object(addr)))
                        .expect("accepted");
                }
            });
        }
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let seen_count = Arc::clone(&seen_count);
            let stop = Arc::clone(&stop);
            scope.spawn(move || {
                let mut worker = queue.register_worker().expect("one slot per worker");
                // Deliveries are counted in batches: at this volume one atomic
                // per item would measure the counter, not the queue. The
                // remainder is flushed whenever the worker runs dry, because
                // the run only ends once the count reaches the total — a
                // remainder held back until after `stop` could never arrive.
                let mut local = 0usize;
                loop {
                    if worker.next_work().is_some() {
                        local += 1;
                        if local == 4096 {
                            seen_count.fetch_add(local, Ordering::SeqCst);
                            local = 0;
                        }
                        continue;
                    }
                    if local > 0 {
                        seen_count.fetch_add(local, Ordering::SeqCst);
                        local = 0;
                    }
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::yield_now();
                }
            });
        }

        wait_for(
            "the nightly soak to deliver every item",
            Duration::from_secs(1800),
            || seen_count.load(Ordering::SeqCst) >= total,
        );
        stop.store(true, Ordering::SeqCst);
    });

    let snapshot = queue.snapshot();
    assert!(snapshot.delivered >= total as u64, "{snapshot:?}");
    assert_eq!(snapshot.counter_underflows, 0);
}

// 34. Lock-order regression guard.
//
//     `flush_all_locals` runs on a collector thread and needs both of a slot's
//     locks; the slot's owner needs both when it refills its private segment
//     from its own shared one. Taking them in opposite orders is a textbook
//     deadlock, and it is invisible to every other test here because it needs
//     those two specific operations to interleave. The whole module therefore
//     takes `injector < shared < private` and this test hammers the pair.
//
//     A deadlock hangs rather than fails, so a watchdog turns it into a loud
//     abort instead of a test run that never returns.
#[test]
fn mark_queue_34_flush_races_local_refill_without_deadlock() {
    let queue = MarkWorkQueue::new(4);
    let epoch = queue.begin_epoch();
    let rounds = 2000usize;
    let done = Arc::new(AtomicBool::new(false));

    {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(120);
            while !done.load(Ordering::SeqCst) {
                if Instant::now() > deadline {
                    eprintln!(
                        "mark_queue_34: flush/refill deadlocked — the lock order \
                         (injector < shared < private) has been broken"
                    );
                    std::process::abort();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
    }

    std::thread::scope(|scope| {
        for _ in 0..4 {
            let queue = Arc::clone(&queue);
            scope.spawn(move || {
                let mut worker = queue.register_worker().expect("one slot per worker");
                for round in 0..rounds {
                    for index in 0..8 {
                        worker.push(object(round * 8 + index + 1)).ok();
                    }
                    // Publish, then immediately pull it back: the refill path is
                    // the one that wants shared-then-private.
                    worker.publish_local(usize::MAX);
                    while worker.next_work().is_some() {}
                }
            });
        }
        let queue = Arc::clone(&queue);
        scope.spawn(move || {
            for _ in 0..rounds {
                queue.flush_all_locals();
                std::thread::yield_now();
            }
        });
    });

    done.store(true, Ordering::SeqCst);

    // Whatever the interleaving, the flushes may have parked work in the
    // injector; nothing may have been lost or miscounted.
    let mut sweeper = queue.register_worker().expect("a free slot");
    while sweeper.next_work().is_some() {}
    drop(sweeper);
    queue.flush_all_locals();
    let mut sweeper = queue.register_worker().expect("a free slot");
    while sweeper.next_work().is_some() {}
    drop(sweeper);

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.epoch, epoch);
    assert_eq!(snapshot.counter_underflows, 0);
    assert!(snapshot.is_drained(), "{snapshot:?}");
}

// 35. Epoch numbers are issued from a counter that only ever moves forward, so
//     ending a cycle and starting another cannot hand out the same number
//     twice. Reuse would be a correctness hole and not a cosmetic one: work and
//     handles left over from the earlier cycle would validate as current.
#[test]
fn mark_queue_35_epoch_numbers_are_never_reissued() {
    let queue = MarkWorkQueue::new(1);

    let first = queue.begin_epoch();
    queue.end_epoch();
    assert!(queue.current_epoch().is_idle());
    let second = queue.begin_epoch();

    assert_ne!(
        first, second,
        "an epoch number must not be reused after end_epoch"
    );

    // Work tagged for the finished cycle is therefore still recognisable as
    // stale, which is the whole point of not reusing the number.
    assert_eq!(
        queue.inject(MarkWork::new(first, object(0x400))),
        Err(RejectReason::StaleEpoch)
    );

    // And it keeps advancing over any number of idle gaps.
    queue.end_epoch();
    let third = queue.begin_epoch();
    assert_ne!(third, first);
    assert_ne!(third, second);
}

// 36. The consequence of 35 for a live handle: a worker that survives a cycle
//     boundary must stay inert. If the epoch number were reused it would wake up
//     believing it was current and push last cycle's object addresses into a
//     mark that has already cleared its bits.
#[test]
fn mark_queue_36_a_handle_stays_stale_across_an_idle_gap() {
    let queue = MarkWorkQueue::new(1);
    queue.begin_epoch();

    let mut worker = queue.register_worker().expect("slot 0");
    worker
        .push(object(0x410))
        .expect("accepted in its own cycle");
    assert!(!worker.is_stale());

    queue.end_epoch();
    queue.begin_epoch();

    assert!(worker.is_stale(), "the handle belongs to a finished cycle");
    assert_eq!(worker.push(object(0x411)), Err(RejectReason::StaleEpoch));
    assert!(worker.next_work().is_none());
    assert_eq!(
        queue.snapshot().outstanding,
        0,
        "the stale push must not have been counted"
    );
}

// 37. The check-then-publish race. A producer validates the epoch, then the
//     collector starts a new cycle, then the producer pushes: the item would
//     belong to a cycle that has already discarded its work and cleared its mark
//     bits. The barrier lines the two threads up on purpose so the window is hit
//     rather than hoped for.
//
//     The invariant is absolute, not statistical: after the race, nothing tagged
//     for the old cycle may be reachable, and the counters must describe reality.
#[test]
fn mark_queue_37_inject_racing_a_new_epoch_never_leaks_stale_work() {
    use std::sync::Barrier;

    const ROUNDS: usize = 200;
    const INJECTS_PER_ROUND: usize = 8;

    for round in 0..ROUNDS {
        let queue = MarkWorkQueue::new(2);
        let old = queue.begin_epoch();
        let barrier = Arc::new(Barrier::new(2));

        let new = std::thread::scope(|scope| {
            {
                let queue = Arc::clone(&queue);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    for index in 0..INJECTS_PER_ROUND {
                        // Tagged for the cycle that is about to end. Accepted or
                        // refused is up to the race; leaking is not.
                        let _ = queue.inject(MarkWork::new(old, object(0x500 + index)));
                    }
                });
            }
            let queue = Arc::clone(&queue);
            let barrier = Arc::clone(&barrier);
            scope
                .spawn(move || {
                    barrier.wait();
                    queue.begin_epoch()
                })
                .join()
                .expect("the collector thread does not panic")
        });

        assert_ne!(old, new);
        assert_eq!(queue.current_epoch(), new);

        // A consumer in the new cycle must find nothing at all: anything the
        // producer got accepted was accepted before the transition and was
        // discarded by it, and anything after was refused.
        let mut worker = queue.register_worker().expect("slot 0");
        if let Some(leaked) = worker.next_work() {
            panic!("round {round}: stale work leaked into epoch {new:?}: {leaked:?}");
        }
        drop(worker);

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.outstanding, 0, "round {round}: {snapshot:?}");
        assert_eq!(
            snapshot.counter_underflows, 0,
            "round {round}: {snapshot:?}"
        );
        assert!(snapshot.is_drained(), "round {round}: {snapshot:?}");
    }
}

// 38. The termination predicate under load, with a producer running alongside
//     the consumers. `is_drained` is read without a lock, so it must not be
//     assembled from several counters that a writer can be caught between.
//
//     The falsifiable claim: every item whose injection *completed before the
//     snapshot began* must already have been consumed if that snapshot says the
//     queue is empty — along with everything those items discovered.
//
//     The producer is deliberately unhurried and the pool deliberately small, so
//     consumers really do go idle between items and the reader gets thousands of
//     chances at the window instead of one. Even so, this is a load test and not
//     a proof: the window a multi-field predicate opens is nanoseconds wide, and
//     re-introducing one does *not* reliably fail here. Perspective 41 is the
//     deterministic guard against that regression; this one shows the accounting
//     survives real traffic.
#[test]
fn mark_queue_38_is_drained_is_never_true_with_work_left() {
    const PRODUCERS: usize = 2;
    const PER_PRODUCER: usize = 250;
    const WORKERS: usize = 3;
    /// Objects each injected root discovers, so consuming one item creates more.
    const FANOUT: usize = 2;

    let (queue, epoch) = started_queue(WORKERS);
    let injected_ok = Arc::new(AtomicUsize::new(0));
    let consumed = Arc::new(AtomicUsize::new(0));
    let producing = Arc::new(AtomicBool::new(true));
    let stop = Arc::new(AtomicBool::new(false));

    std::thread::scope(|scope| {
        for producer in 0..PRODUCERS {
            let queue = Arc::clone(&queue);
            let injected_ok = Arc::clone(&injected_ok);
            scope.spawn(move || {
                for index in 0..PER_PRODUCER {
                    let shard = (producer * PER_PRODUCER + index) as u32;
                    queue
                        .inject(MarkWork::new(
                            epoch,
                            MarkWorkItem::RootShard(RootShardId(shard)),
                        ))
                        .expect("the cycle runs for the whole test");
                    // Counted only once the item is fully inserted, so a reader
                    // that loads this before its snapshot knows the queue held
                    // the item when the snapshot started.
                    injected_ok.fetch_add(1, Ordering::SeqCst);
                    std::thread::yield_now();
                }
            });
        }

        for _ in 0..WORKERS {
            let queue = Arc::clone(&queue);
            let consumed = Arc::clone(&consumed);
            let stop = Arc::clone(&stop);
            scope.spawn(move || {
                let mut worker = queue.register_worker().expect("one slot per worker");
                while !stop.load(Ordering::SeqCst) {
                    let Some(work) = worker.next_work() else {
                        std::thread::yield_now();
                        continue;
                    };
                    consumed.fetch_add(1, Ordering::SeqCst);
                    if let MarkWorkItem::RootShard(RootShardId(shard)) = work.item {
                        for index in 0..FANOUT {
                            worker
                                .push(object(0x1_0000 + shard as usize * FANOUT + index))
                                .expect("the cycle is still running");
                        }
                    }
                }
            });
        }

        let queue = Arc::clone(&queue);
        let injected_ok = Arc::clone(&injected_ok);
        let consumed = Arc::clone(&consumed);
        let producing = Arc::clone(&producing);
        let stop = Arc::clone(&stop);
        scope.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            let total = PRODUCERS * PER_PRODUCER;
            loop {
                // Read what has definitely been injected BEFORE sampling, so the
                // two readings cannot be blamed on ordering.
                let injected_before = injected_ok.load(Ordering::SeqCst);
                let snapshot = queue.snapshot();
                if snapshot.is_drained() {
                    let done = consumed.load(Ordering::SeqCst);
                    assert!(
                        done >= injected_before * (1 + FANOUT),
                        "is_drained reported an empty queue while {} of {} injected items \
                         (and their {FANOUT}-way fan-out) were still unconsumed: {snapshot:?}",
                        injected_before * (1 + FANOUT) - done,
                        injected_before,
                    );
                    if injected_before == total {
                        break;
                    }
                }
                assert!(Instant::now() < deadline, "marking did not terminate");
                std::thread::yield_now();
            }
            producing.store(false, Ordering::SeqCst);
            stop.store(true, Ordering::SeqCst);
        });
    });

    assert!(
        !producing.load(Ordering::SeqCst),
        "the watcher ran to the end"
    );
    let snapshot = queue.snapshot();
    assert_eq!(
        consumed.load(Ordering::SeqCst),
        PRODUCERS * PER_PRODUCER * (1 + FANOUT),
        "{snapshot:?}"
    );
    assert_eq!(snapshot.counter_underflows, 0);
    assert_eq!(snapshot.abandoned, 0);
    assert_eq!(queue.end_epoch(), 0, "a terminated cycle leaves nothing");
}

// 39. A consumer that takes an item and then dies without relinquishing it has
//     lost that object: this handle gave ownership away and cannot get it back.
//     The cycle must still be able to finish, and the mistake must be a number
//     somebody can look at rather than an object that quietly went unmarked.
#[test]
fn mark_queue_39_abandoning_an_item_in_hand_is_counted() {
    let (queue, epoch) = started_queue(1);
    queue
        .inject(MarkWork::new(epoch, object(0x600)))
        .expect("accepted");

    {
        let mut worker = queue.register_worker().expect("slot 0");
        let work = worker.next_work().expect("the injected item");
        assert_eq!(queue.snapshot().outstanding, 1);
        // Dropped on the floor: no `relinquish`, no completion.
        drop(work);
    }

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.abandoned, 1);
    assert_eq!(snapshot.counter_underflows, 0);
    assert_eq!(
        snapshot.outstanding, 0,
        "an abandoned item must not strand the cycle forever"
    );
    assert!(snapshot.is_drained());
}

// 40. `relinquish` is documented as moving an item from a hand back to the
//     queue, which leaves the count alone. Work the consumer was never handed is
//     a new insertion instead, and has to be counted as one or the queue would
//     hold an item it does not know about.
#[test]
fn mark_queue_40_relinquishing_unheld_work_counts_as_an_insertion() {
    let (queue, epoch) = started_queue(1);
    let mut worker = queue.register_worker().expect("slot 0");

    worker.relinquish(MarkWork::new(epoch, object(0x610)));

    let snapshot = queue.snapshot();
    assert_eq!(snapshot.outstanding, 1);
    assert_eq!(snapshot.published, 1);
    assert!(!snapshot.is_drained());

    assert_eq!(drain(&mut worker), vec![0x610]);
    let snapshot = queue.snapshot();
    assert_eq!(snapshot.outstanding, 0);
    assert_eq!(snapshot.counter_underflows, 0);
    assert!(snapshot.is_drained());
}

// 41. The structural half of 38, and the one that fails deterministically.
//
//     A predicate spread over several counters is unsound no matter how the
//     writes are ordered, because the reads happen one at a time: nothing stops
//     a writer from moving work between two of them mid-read. The fix is not
//     better ordering, it is fewer fields — so pin that down. Only `outstanding`
//     may change the answer; every other field is advisory, and re-admitting one
//     into the predicate (say `&& active_consumers == 0`) must fail here.
#[test]
fn mark_queue_41_is_drained_depends_on_exactly_one_counter() {
    let empty = MarkWorkQueue::new(1).snapshot();
    assert!(empty.is_drained(), "a fresh queue holds nothing");

    /// Sets one advisory field to a non-zero value, leaving the rest alone.
    type Poke = fn(&mut MarkQueueSnapshot);

    let advisory: Vec<(&str, Poke)> = vec![
        ("epoch", |s| s.epoch = MarkEpoch(7)),
        ("published", |s| s.published = 3),
        ("hidden", |s| s.hidden = 3),
        ("active_consumers", |s| s.active_consumers = 3),
        ("registered_workers", |s| s.registered_workers = 3),
        ("registered_assists", |s| s.registered_assists = 3),
        ("injected", |s| s.injected = 3),
        ("delivered", |s| s.delivered = 3),
        ("republished", |s| s.republished = 3),
        ("steals", |s| s.steals = 3),
        ("steal_attempts", |s| s.steal_attempts = 3),
        ("stale_rejected", |s| s.stale_rejected = 3),
        ("quarantined", |s| s.quarantined = 3),
        ("counter_underflows", |s| s.counter_underflows = 3),
        ("abandoned", |s| s.abandoned = 3),
    ];
    for (field, mutate) in advisory {
        let mut snapshot = empty;
        mutate(&mut snapshot);
        assert!(
            snapshot.is_drained(),
            "`{field}` is advisory and must not enter the termination predicate"
        );
    }

    let mut snapshot = empty;
    snapshot.outstanding = 1;
    assert!(
        !snapshot.is_drained(),
        "outstanding work is the whole point"
    );
}
