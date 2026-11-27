use super::*;

/// Worker-owned FIFO deques with concurrent inbound queues.
/// Foreign or off-drive publishers write to a worker's inbox. The active owner
/// transfers a bounded batch into its deque; thieves may steal from either
/// source. The global injection queue accepts concurrent foreign publishers.
#[derive(Debug)]
pub(super) struct RunQueues {
    pub(super) parking: Arc<parking::Parking>,
    pub(super) retry_observation: RuntimeRetryObservation,
    pub(super) locals: Vec<LocalRunQueue>,
    pub(super) global: Injector<RuntimeTaskId>,
    pub(super) global_len: AtomicUsize,
    pub(super) metrics: crate::observability::RunQueueMetrics,
    pub(super) worker_metrics: Vec<crate::observability::RunQueueMetrics>,
    pub(super) observation: RuntimeQueueObservation,
}

/// Keep independently updated worker state apart. The owner is taken once per
/// drive and returned on exit; successful owner pops never take this mutex.
/// Every other thread uses the inbox or the lock-free stealer.
#[repr(align(128))]
#[derive(Debug)]
pub(super) struct LocalRunQueue {
    pub(super) retry_observation: RuntimeRetryObservation,
    pub(super) inbox: Injector<RuntimeTaskId>,
    pub(super) owner: Mutex<Option<Worker<RuntimeTaskId>>>,
    pub(super) stealer: Stealer<RuntimeTaskId>,
    /// Tasks spawned by this worker's polls, newest first. A spawn tree then
    /// runs depth-first, so its live tasks stay near its depth times fan-out
    /// instead of its size, while thieves take the oldest (largest) subtrees.
    /// Wakes, yields and preemptions stay on the FIFO deque above.
    pub(super) spawned_owner: Mutex<Option<Worker<RuntimeTaskId>>>,
    pub(super) spawned_stealer: Stealer<RuntimeTaskId>,
    pub(super) len: AtomicUsize,
    pub(super) prefer_global: AtomicBool,
}

impl LocalRunQueue {
    pub(super) fn new(retry_observation: RuntimeRetryObservation) -> Self {
        let owner = Worker::new_fifo();
        let stealer = owner.stealer();
        let spawned = Worker::new_lifo();
        let spawned_stealer = spawned.stealer();
        Self {
            retry_observation,
            inbox: Injector::new(),
            owner: Mutex::new(Some(owner)),
            stealer,
            spawned_owner: Mutex::new(Some(spawned)),
            spawned_stealer,
            len: AtomicUsize::new(0),
            prefer_global: AtomicBool::new(false),
        }
    }

    pub(super) fn looks_empty(&self) -> bool {
        self.len.load(Ordering::Acquire) == 0
    }

    pub(super) fn publish(&self, id: RuntimeTaskId) -> usize {
        // Count before publication so an idle-stop snapshot cannot miss an
        // in-progress publisher. A failed early probe only causes a retry.
        let backlog = self.len.fetch_add(1, Ordering::AcqRel) + 1;
        let published_to_owner = LOCAL_QUEUE_OWNER.with(|slot| {
            let slot = slot.borrow();
            if let Some(owner) = slot
                .as_ref()
                .filter(|owner| owner.queue == self as *const _ as usize)
                && self.inbox.is_empty()
            {
                owner.worker.push(id);
                true
            } else {
                false
            }
        });
        if !published_to_owner {
            self.inbox.push(id);
        }
        backlog
    }

    /// Publish a task spawned by the current poll. False when this thread does
    /// not own the queue; the caller then uses the global queue.
    pub(super) fn publish_spawned(&self, id: RuntimeTaskId) -> bool {
        LOCAL_QUEUE_OWNER.with(|slot| {
            let slot = slot.borrow();
            let Some(owner) = slot
                .as_ref()
                .filter(|owner| owner.queue == self as *const _ as usize)
            else {
                return false;
            };
            // Count before publication, as in `publish`.
            self.len.fetch_add(1, Ordering::AcqRel);
            owner.spawned.push(id);
            true
        })
    }

    pub(super) fn steal(&self) -> Option<RuntimeTaskId> {
        let id = self.steal_without_count();
        if id.is_some() {
            self.len.fetch_sub(1, Ordering::AcqRel);
        }
        id
    }

    /// Steal for the worker that owns `thief`. Half of the victim's oldest
    /// spawns move at once: the leaves of a spawn tree last a microsecond each,
    /// so a thief taking one at a time would idle and park after every task.
    pub(super) fn steal_into(&self, thief: &Self) -> Option<RuntimeTaskId> {
        let batched = LOCAL_QUEUE_OWNER.with(|slot| {
            let slot = slot.borrow();
            let owner = slot
                .as_ref()
                .filter(|owner| owner.queue == thief as *const _ as usize)?;
            let id = loop {
                match self.spawned_stealer.steal_batch_and_pop(&owner.stolen) {
                    Steal::Success(id) => break id,
                    Steal::Empty => return None,
                    Steal::Retry => std::hint::spin_loop(),
                }
            };
            // Count at the thief before the victim gives its count up, so an
            // idle-stop snapshot cannot miss a task in flight between them.
            let moved = owner.stolen.len();
            thief.len.fetch_add(moved, Ordering::AcqRel);
            while let Some(stolen) = owner.stolen.pop() {
                owner.spawned.push(stolen);
            }
            self.len.fetch_sub(moved + 1, Ordering::AcqRel);
            Some(id)
        });
        batched.or_else(|| self.steal())
    }

    pub(super) fn pop(&self) -> Option<RuntimeTaskId> {
        let from_owner = LOCAL_QUEUE_OWNER.with(|slot| {
            let slot = slot.borrow();
            let owner = slot
                .as_ref()
                .filter(|context| context.queue == self as *const _ as usize)?;
            let fifo = || {
                owner.worker.pop().or_else(|| {
                    steal_retry_batch(&self.inbox, &owner.worker, &self.retry_observation)
                })
            };
            let oldest_spawned = || steal_retry(&self.spawned_stealer, &self.retry_observation);
            let turn = owner.pops.get().wrapping_add(1);
            owner.pops.set(turn);
            // Newest-first alone could starve older spawns and the FIFO deque
            // behind an endless spawn chain, so each gets a bounded turn.
            if turn % OLDEST_SPAWN_TURN == 0 {
                oldest_spawned().or_else(fifo)
            } else if turn % 2 == 0 {
                fifo().or_else(|| owner.spawned.pop())
            } else {
                owner.spawned.pop().or_else(fifo)
            }
        });
        let id = from_owner.or_else(|| self.steal_without_count());
        if id.is_some() {
            self.len.fetch_sub(1, Ordering::AcqRel);
        }
        id
    }

    pub(super) fn steal_without_count(&self) -> Option<RuntimeTaskId> {
        steal_retry(&self.spawned_stealer, &self.retry_observation)
            .or_else(|| steal_retry(&self.stealer, &self.retry_observation))
            .or_else(|| steal_retry(&self.inbox, &self.retry_observation))
    }
}

/// Every this many owner pops the oldest spawned task goes first. It bounds
/// starvation inside the lane; each such turn starts a subtree out of depth
/// order, so frequent turns would give up the bounded live set.
pub(super) const OLDEST_SPAWN_TURN: u32 = 1024;

pub(super) fn steal_retry_batch<T>(
    inbox: &Injector<T>,
    owner: &Worker<T>,
    observation: &RuntimeRetryObservation,
) -> Option<T> {
    let mut probe = observation.begin(1);
    loop {
        match inbox.steal_batch_and_pop(owner) {
            Steal::Success(id) => return Some(id),
            Steal::Empty => return None,
            Steal::Retry => {
                probe.retry();
                std::hint::spin_loop();
            }
        }
    }
}

pub(super) fn steal_retry<T>(
    stealer: &impl FnSteal<T>,
    observation: &RuntimeRetryObservation,
) -> Option<T> {
    let mut probe = observation.begin(0);
    loop {
        match stealer.steal_one() {
            Steal::Success(id) => return Some(id),
            Steal::Empty => return None,
            Steal::Retry => {
                probe.retry();
                std::hint::spin_loop();
            }
        }
    }
}

pub(super) trait FnSteal<T> {
    fn steal_one(&self) -> Steal<T>;
}

impl<T> FnSteal<T> for Injector<T> {
    fn steal_one(&self) -> Steal<T> {
        self.steal()
    }
}
impl<T> FnSteal<T> for Stealer<T> {
    fn steal_one(&self) -> Steal<T> {
        self.steal()
    }
}

pub(super) struct LocalOwner {
    pub(super) queue: usize,
    pub(super) worker: Worker<RuntimeTaskId>,
    pub(super) spawned: Worker<RuntimeTaskId>,
    /// Private landing buffer that counts a batch steal exactly before the
    /// batch becomes visible to other thieves; empty between steals.
    pub(super) stolen: Worker<RuntimeTaskId>,
    pub(super) pops: Cell<u32>,
}
thread_local! {
    pub(super) static LOCAL_QUEUE_OWNER: RefCell<Option<LocalOwner>> = const { RefCell::new(None) };
}

pub(super) struct LocalOwnerGuard<'a> {
    pub(super) queue: &'a LocalRunQueue,
    pub(super) active: bool,
}

impl Drop for LocalOwnerGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let owner =
                LOCAL_QUEUE_OWNER.with(|slot| slot.borrow_mut().take().expect("owner missing"));
            *self.queue.owner.lock().unwrap_or_else(|e| e.into_inner()) = Some(owner.worker);
            *self
                .queue
                .spawned_owner
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(owner.spawned);
        }
    }
}

impl RunQueues {
    pub(super) fn new(worker_count: usize) -> Self {
        Self::with_parking(worker_count, Arc::clone(&crate::runtime::current().parking))
    }

    pub(super) fn with_parking(worker_count: usize, parking: Arc<parking::Parking>) -> Self {
        let worker_count = worker_count.max(1);
        let retry_observation = RuntimeRetryObservation::default();
        Self {
            parking,
            observation: RuntimeQueueObservation::default(),
            locals: (0..worker_count)
                .map(|_| LocalRunQueue::new(retry_observation.clone()))
                .collect(),
            retry_observation,
            global: Injector::new(),
            global_len: AtomicUsize::new(0),
            metrics: crate::observability::RunQueueMetrics::default(),
            worker_metrics: (0..worker_count)
                .map(|_| crate::observability::RunQueueMetrics::default())
                .collect(),
        }
    }

    pub(super) fn with_owner<R>(&self, worker: usize, f: impl FnOnce() -> R) -> R {
        let Some(queue) = self.locals.get(worker) else {
            return f();
        };
        let nested = LOCAL_QUEUE_OWNER.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|owner| owner.queue == queue as *const _ as usize)
        });
        if nested {
            return f();
        }
        if LOCAL_QUEUE_OWNER.with(|slot| slot.borrow().is_some()) {
            return f();
        }
        let taken = queue.owner.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(owner) = taken else { return f() };
        let spawned = queue
            .spawned_owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("the spawn lane travels with the queue owner");
        LOCAL_QUEUE_OWNER.with(|slot| {
            *slot.borrow_mut() = Some(LocalOwner {
                queue: queue as *const _ as usize,
                worker: owner,
                spawned,
                stolen: Worker::new_lifo(),
                pops: Cell::new(0),
            });
        });
        let _guard = LocalOwnerGuard {
            queue,
            active: true,
        };
        f()
    }

    pub(super) fn worker_count(&self) -> usize {
        self.locals.len()
    }

    pub(super) fn notify_idle(&self) {
        self.observation.idle_notification();
        self.parking.notify_idle();
    }

    pub(super) fn push_global(&self, id: RuntimeTaskId) {
        self.push_global_batch(std::slice::from_ref(&id));
    }

    pub(super) fn push_global_batch(&self, ids: &[RuntimeTaskId]) {
        self.publish_global_batch(ids, true);
    }

    pub(super) fn push_spawned(&self, id: RuntimeTaskId) {
        let defer = SCHED_RUN_DEPTH.with(|depth| depth.get() > 0);
        if defer {
            let worker = current_worker();
            if let Some(queue) = self.locals.get(worker)
                && queue.publish_spawned(id)
            {
                SPAWN_NOTIFICATION_PENDING.with(|pending| pending.set(true));
                self.worker_metrics[worker]
                    .local_pushes
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        self.publish_global_batch(std::slice::from_ref(&id), !defer);
        if defer {
            SPAWN_NOTIFICATION_PENDING.with(|pending| pending.set(true));
        }
    }

    pub(super) fn publish_global_batch(&self, ids: &[RuntimeTaskId], notify: bool) {
        if ids.is_empty() {
            return;
        }
        self.observation.before_global_publish();
        let previous_len = self.global_len.fetch_add(ids.len(), Ordering::AcqRel);
        for &id in ids {
            self.global.push(id);
        }
        let backlog = previous_len + ids.len();
        if notify {
            let threshold = if SCHED_RUN_DEPTH.with(|depth| depth.get() > 0) {
                2
            } else {
                1
            };
            if previous_len < threshold && backlog >= threshold {
                self.notify_idle();
            }
        }
        self.metrics
            .global_pushes
            .fetch_add(ids.len() as u64, Ordering::Relaxed);
    }

    pub(super) fn push_local(&self, worker: usize, id: RuntimeTaskId) {
        match self.locals.get(worker) {
            Some(queue) => {
                let backlog = queue.publish(id);
                if SCHED_RUN_DEPTH.with(|depth| depth.get() > 0) && worker == current_worker() {
                    if backlog > 1 {
                        // Requeueing the current poll already publishes the
                        // deferred batch; do not notify twice at its boundary.
                        LOCAL_WAKE_NOTIFICATION_PENDING.with(|pending| pending.set(false));
                    }
                    self.parking.notify_local(backlog);
                } else {
                    self.notify_idle();
                }
                self.worker_metrics[worker]
                    .local_pushes
                    .fetch_add(1, Ordering::Relaxed);
            }
            None => self.push_global(id),
        }
    }

    /// Whether more than `waiting` workers other than `worker` have an empty
    /// local queue. Stops at the first `waiting + 1` such peers: O(workers).
    pub(super) fn idle_peers_exceed(&self, worker: usize, waiting: usize) -> bool {
        self.locals
            .iter()
            .enumerate()
            .filter(|&(index, queue)| index != worker && queue.looks_empty())
            .nth(waiting)
            .is_some()
    }

    /// Make the worker's next pop local-first, so a task it just spread to the
    /// shared queue goes to an idle peer before this worker can take it back.
    pub(super) fn defer_global_pop(&self, worker: usize) {
        if let Some(queue) = self.locals.get(worker) {
            queue.prefer_global.store(false, Ordering::Relaxed);
        }
    }

    /// Return a pinned task to its owner and wake only that owner: an idle
    /// thief would just steal it and reroute it again.
    pub(super) fn push_rerouted(&self, owner: usize, id: RuntimeTaskId) {
        match self.locals.get(owner) {
            Some(queue) => {
                queue.publish(id);
                self.observation.owner_notification();
                self.parking.notify_worker(owner);
                self.worker_metrics[owner]
                    .local_pushes
                    .fetch_add(1, Ordering::Relaxed);
            }
            // A drive can run more workers than this scheduler has local
            // queues (pinned stacks raise the pool size). Such an owner takes
            // the task from the shared queue, so it must be woken by index: the
            // generic backlog notification is suppressed for a single task
            // inside a run, which would leave the owner parked while every
            // other worker keeps rerouting the task.
            None => {
                self.publish_global_batch(std::slice::from_ref(&id), false);
                self.observation.owner_notification();
                self.parking.notify_worker(owner);
            }
        }
    }

    pub(super) fn push_woken_batch(&self, ids: &[RuntimeTaskId]) {
        if ids.is_empty() {
            return;
        }
        let worker = SCHED_RUN_DEPTH.with(|depth| (depth.get() > 0).then(current_worker));
        if let Some(worker) = worker.filter(|&worker| worker < self.locals.len()) {
            let queue = &self.locals[worker];
            let previous = queue.len.load(Ordering::Acquire);
            for &id in ids {
                queue.publish(id);
            }
            self.worker_metrics[worker]
                .local_pushes
                .fetch_add(ids.len() as u64, Ordering::Relaxed);
            let backlog = previous + ids.len();
            // A small channel fan-in can wake several producers in one poll.
            // Coalesce thief notifications until its boundary; all tasks are
            // already published and stealable. Large fanouts notify promptly.
            if backlog >= 32 {
                self.parking.notify_local(backlog);
            } else if backlog > 1 {
                LOCAL_WAKE_NOTIFICATION_PENDING.with(|pending| pending.set(true));
            }
        } else {
            self.push_global_batch(ids);
        }
    }

    pub(super) fn flush_local_wake_notification(&self, worker: usize) -> bool {
        if LOCAL_WAKE_NOTIFICATION_PENDING.with(|pending| pending.replace(false)) {
            let backlog = self
                .locals
                .get(worker)
                .map_or(0, |queue| queue.len.load(Ordering::Acquire));
            self.parking.notify_local(backlog);
            return backlog > 1;
        }
        false
    }

    #[cfg(test)]
    pub(super) fn push_local_front(&self, worker: usize, id: RuntimeTaskId) {
        self.push_local(worker, id);
    }
    #[cfg(test)]
    pub(super) fn force_push_global(&self, id: RuntimeTaskId) {
        self.push_global(id);
    }

    pub(super) fn pop_global(
        &self,
        worker: usize,
        metrics: &crate::observability::RunQueueMetrics,
        refill: bool,
    ) -> Option<RuntimeTaskId> {
        metrics.global_pop_attempts.fetch_add(1, Ordering::Relaxed);
        if self.global_len.load(Ordering::Acquire) == 0 {
            return None;
        }
        let id = steal_retry(&self.global, &self.retry_observation)?;
        self.global_len.fetch_sub(1, Ordering::AcqRel);
        if refill
            && self.locals.len() > 1
            && let Some(local) = self.locals.get(worker)
            && local.looks_empty()
        {
            LOCAL_QUEUE_OWNER.with(|slot| {
                let slot = slot.borrow();
                if let Some(owner) = slot
                    .as_ref()
                    .filter(|ctx| ctx.queue == local as *const _ as usize)
                {
                    let count = (self.global_len.load(Ordering::Acquire) / 2).min(31);
                    let mut transferred = 0;
                    for _ in 0..count {
                        let Some(next) = steal_retry(&self.global, &self.retry_observation) else {
                            break;
                        };
                        self.global_len.fetch_sub(1, Ordering::AcqRel);
                        local.len.fetch_add(1, Ordering::AcqRel);
                        owner.worker.push(next);
                        transferred += 1;
                    }
                    if transferred > 0 {
                        metrics
                            .local_pushes
                            .fetch_add(transferred, Ordering::Relaxed);
                        self.notify_idle();
                    }
                }
            });
        }
        metrics.global_pop_hits.fetch_add(1, Ordering::Relaxed);
        Some(id)
    }

    pub(super) fn pop_for_worker(&self, worker: usize) -> Option<RuntimeTaskId> {
        let metrics = self.worker_metrics.get(worker).unwrap_or(&self.metrics);
        let prefer_global = self
            .locals
            .get(worker)
            .map(|queue| queue.prefer_global.fetch_xor(true, Ordering::Relaxed))
            .unwrap_or(true);
        if prefer_global {
            if let Some(id) = self.pop_global(worker, metrics, false) {
                return Some(id);
            }
            if let Some(queue) = self.locals.get(worker).filter(|queue| !queue.looks_empty())
                && let Some(id) = queue.pop()
            {
                metrics.local_pop_hits.fetch_add(1, Ordering::Relaxed);
                return Some(id);
            }
        } else {
            if let Some(queue) = self.locals.get(worker).filter(|queue| !queue.looks_empty())
                && let Some(id) = queue.pop()
            {
                metrics.local_pop_hits.fetch_add(1, Ordering::Relaxed);
                return Some(id);
            }
            if let Some(id) = self.pop_global(worker, metrics, true) {
                return Some(id);
            }
        }
        metrics.steal_attempts.fetch_add(1, Ordering::Relaxed);
        for offset in 1..self.locals.len() {
            let victim = (worker + offset) % self.locals.len();
            let queue = &self.locals[victim];
            if queue.looks_empty() {
                continue;
            }
            if let Some(id) = queue.steal_into(&self.locals[worker % self.locals.len()]) {
                metrics.steal_successes.fetch_add(1, Ordering::Relaxed);
                return Some(id);
            }
        }
        metrics.steal_failures.fetch_add(1, Ordering::Relaxed);
        None
    }

    pub(super) fn metrics_snapshot(&self) -> crate::observability::RunQueueMetricsSnapshot {
        let mut snapshot = self.metrics.snapshot();
        for metrics in &self.worker_metrics {
            snapshot.add(metrics.snapshot());
        }
        snapshot
    }

    pub(super) fn remove(&self, id: RuntimeTaskId) -> bool {
        if Self::remove_from(
            &self.retry_observation,
            &self.global,
            &self.global,
            self.global.len(),
            &self.global_len,
            id,
        ) {
            return true;
        }
        self.locals.iter().any(|queue| {
            // Only an accessible owner can rotate its deque in place. Never
            // drain a foreign active owner's deque into its newer inbox.
            let removed_from_owner = LOCAL_QUEUE_OWNER.with(|slot| {
                let local = slot.borrow();
                if let Some(owner) = local
                    .as_ref()
                    .filter(|owner| owner.queue == queue as *const _ as usize)
                {
                    Some(Self::remove_from_owner(&owner.worker, &queue.len, id))
                } else {
                    let owner = queue.owner.lock().unwrap_or_else(|e| e.into_inner());
                    owner
                        .as_ref()
                        .map(|worker| Self::remove_from_owner(worker, &queue.len, id))
                }
            });
            // Physical removal is best-effort when another drive owns this
            // queue. Leave both sources untouched; normal claim processing
            // discards tokens made stale by placeholder state transitions.
            let Some(removed_from_owner) = removed_from_owner else {
                return false;
            };
            removed_from_owner
                || Self::remove_from(
                    &self.retry_observation,
                    &queue.inbox,
                    &queue.inbox,
                    queue.inbox.len(),
                    &queue.len,
                    id,
                )
        })
    }

    pub(super) fn remove_from_owner(
        worker: &Worker<RuntimeTaskId>,
        len: &AtomicUsize,
        id: RuntimeTaskId,
    ) -> bool {
        let mut found = false;
        for _ in 0..worker.len() {
            let Some(next) = worker.pop() else { break };
            if next == id && !found {
                found = true;
                len.fetch_sub(1, Ordering::AcqRel);
            } else {
                worker.push(next);
            }
        }
        found
    }

    // Placeholder-only maintenance. Length stays elevated while retained
    // tokens are in transit, preserving the idle-stop invariant.
    pub(super) fn remove_from(
        observation: &RuntimeRetryObservation,
        source: &impl FnSteal<RuntimeTaskId>,
        destination: &Injector<RuntimeTaskId>,
        count: usize,
        len: &AtomicUsize,
        id: RuntimeTaskId,
    ) -> bool {
        let mut found = false;
        for _ in 0..count {
            let Some(next) = steal_retry(source, observation) else {
                break;
            };
            if next == id && !found {
                found = true;
                len.fetch_sub(1, Ordering::AcqRel);
            } else {
                destination.push(next);
            }
        }
        found
    }

    pub(super) fn len(&self) -> usize {
        self.global_len.load(Ordering::Acquire)
            + self
                .locals
                .iter()
                .map(|queue| queue.len.load(Ordering::Acquire))
                .sum::<usize>()
    }

    #[cfg(test)]
    pub(super) fn contains(&self, id: RuntimeTaskId) -> bool {
        self.snapshot().contains(&id)
    }
    #[cfg(test)]
    pub(super) fn snapshot(&self) -> Vec<RuntimeTaskId> {
        fn inspect(
            observation: &RuntimeRetryObservation,
            source: &impl FnSteal<RuntimeTaskId>,
            dest: &Injector<RuntimeTaskId>,
            n: usize,
            out: &mut Vec<RuntimeTaskId>,
        ) {
            let start = out.len();
            for _ in 0..n {
                let Some(id) = steal_retry(source, observation) else {
                    break;
                };
                out.push(id);
            }
            for &id in &out[start..] {
                dest.push(id);
            }
        }
        let mut ids = Vec::with_capacity(self.len());
        inspect(
            &self.retry_observation,
            &self.global,
            &self.global,
            self.global.len(),
            &mut ids,
        );
        for queue in &self.locals {
            // Test-only, quiescent inspection: restore each source in place.
            // A Stealer cannot restore an active foreign owner's deque.
            let inspect_owner = |worker: &Worker<RuntimeTaskId>, ids: &mut Vec<RuntimeTaskId>| {
                for _ in 0..worker.len() {
                    let id = worker.pop().expect("snapshot requires quiescent queues");
                    ids.push(id);
                    worker.push(id);
                }
            };
            // Popping a LIFO deque and pushing back would see one entry forever.
            let inspect_spawned = |worker: &Worker<RuntimeTaskId>, ids: &mut Vec<RuntimeTaskId>| {
                let newest_first: Vec<_> = std::iter::from_fn(|| worker.pop()).collect();
                ids.extend(newest_first.iter().copied());
                newest_first
                    .into_iter()
                    .rev()
                    .for_each(|id| worker.push(id));
            };
            LOCAL_QUEUE_OWNER.with(|slot| {
                let local = slot.borrow();
                if let Some(owner) = local
                    .as_ref()
                    .filter(|owner| owner.queue == queue as *const _ as usize)
                {
                    inspect_owner(&owner.worker, &mut ids);
                    inspect_spawned(&owner.spawned, &mut ids);
                } else {
                    let owner = queue.owner.lock().unwrap_or_else(|e| e.into_inner());
                    let worker = owner
                        .as_ref()
                        .expect("snapshot requires inactive or current-thread owners");
                    inspect_owner(worker, &mut ids);
                    let spawned = queue
                        .spawned_owner
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(spawned) = spawned.as_ref() {
                        inspect_spawned(spawned, &mut ids);
                    }
                }
            });
            inspect(
                &self.retry_observation,
                &queue.inbox,
                &queue.inbox,
                queue.inbox.len(),
                &mut ids,
            );
        }
        ids
    }

    #[cfg(test)]
    pub(super) fn clear(&self) {
        while steal_retry(&self.global, &self.retry_observation).is_some() {
            self.global_len.fetch_sub(1, Ordering::AcqRel);
        }
        for queue in &self.locals {
            while queue.steal_without_count().is_some() {
                queue.len.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
}
