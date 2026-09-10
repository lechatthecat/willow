//! Synchronous heap continuations for recursive compiler algorithms.
//!
//! Rust async state machines retain borrows and local control-flow state. This
//! executor polls only the current leaf: a nested continuation registers a
//! child and returns Pending instead of recursively polling that child.
use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

#[derive(Clone, Copy)]
struct Task {
    data: *mut (),
    poll: unsafe fn(*mut (), &mut Context<'_>) -> Poll<()>,
    child: unsafe fn(*mut ()) -> Option<Task>,
    cancel: unsafe fn(*mut ()),
}
struct Executor {
    child: Option<Task>,
}
thread_local! {
    static EXECUTOR: Cell<*mut Executor> = const { Cell::new(std::ptr::null_mut()) };
}
struct State<'a, T> {
    future: Option<Pin<Box<dyn Future<Output = T> + 'a>>>,
    result: Option<T>,
    child: Option<Task>,
}
/// An internal compiler continuation. It must run through [`run`]; external
/// asynchronous I/O is intentionally unsupported by this synchronous executor.
pub(crate) struct Continuation<'a, T> {
    state: Option<Box<State<'a, T>>>,
}
impl<'a, T> Continuation<'a, T> {
    /// Build one audited synchronous compiler state machine.
    ///
    /// # Safety
    /// Every child still registered when a poll returns Pending must remain
    /// exclusively owned by the suspended parent until readiness/cancellation.
    /// Its state cannot be externally moved out, dropped, or re-polled while
    /// scheduled. Awaiting fresh child continuations directly in an async state
    /// machine satisfies this contract. Arbitrary user-provided Futures do not.
    pub(crate) unsafe fn new(future: impl Future<Output = T> + 'a) -> Self {
        Self {
            state: Some(Box::new(State {
                future: Some(Box::pin(future)),
                result: None,
                child: None,
            })),
        }
    }
}
// Moving the wrapper never moves its boxed state or pinned future.
impl<T> Unpin for Continuation<'_, T> {}
unsafe fn poll_state<T>(data: *mut (), context: &mut Context<'_>) -> Poll<()> {
    // SAFETY: the executor retains the owning parent until this task finishes.
    // The box is stable; only its active leaf is polled. No parent is polled
    // concurrently, and a parent's next poll is the only operation that can
    // consume/drop this child. The erased lifetime lasts for that parent poll.
    let state = unsafe { &mut *data.cast::<State<'_, T>>() };
    state.child = None;
    match state
        .future
        .as_mut()
        .expect("completed continuation polled")
        .as_mut()
        .poll(context)
    {
        Poll::Ready(value) => {
            state.result = Some(value);
            state.future = None;
            Poll::Ready(())
        }
        Poll::Pending => {
            state.child = EXECUTOR.with(|slot| {
                // SAFETY: the executor is live throughout this poll.
                unsafe { (*slot.get()).child }
            });
            Poll::Pending
        }
    }
}
impl<T> Future for Continuation<'_, T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<T> {
        let state = self
            .get_mut()
            .state
            .as_mut()
            .expect("consumed continuation");
        if let Some(value) = state.result.take() {
            return Poll::Ready(value);
        }
        EXECUTOR.with(|slot| {
            let executor = slot.get();
            assert!(
                !executor.is_null(),
                "compiler continuation requires compiler_stack::run"
            );
            // SAFETY: run installs its live executor for each synchronous poll.
            // Child polls only register work; they never poll another task.
            let executor = unsafe { &mut *executor };
            assert!(
                executor.child.is_none(),
                "multiple active compiler children"
            );
            executor.child = Some(Task {
                data: (&mut **state as *mut State<'_, T>).cast(),
                poll: poll_state::<T>,
                child: child_state::<T>,
                cancel: cancel_state::<T>,
            });
        });
        Poll::Pending
    }
}
unsafe fn child_state<T>(data: *mut ()) -> Option<Task> {
    // SAFETY: called only for a live state retained by its suspended parent.
    unsafe { (*data.cast::<State<'_, T>>()).child }
}
unsafe fn cancel_state<T>(data: *mut ()) {
    // SAFETY: descendants have already been cancelled, deepest first. Their
    // boxes remain owned by this frame until normal future destruction below.
    let state = unsafe { &mut *data.cast::<State<'_, T>>() };
    state.child = None;
    state.future = None;
}
impl<T> Drop for Continuation<'_, T> {
    fn drop(&mut self) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let address = (&mut **state as *mut State<'_, T>).cast::<()>();
        EXECUTOR.with(|slot| {
            let executor = slot.get();
            if !executor.is_null() {
                // SAFETY: run retains the active executor. A custom Future may
                // poll then drop a temporary child before yielding; remove that
                // registration before its state is freed.
                let executor = unsafe { &mut *executor };
                if executor.child.is_some_and(|task| task.data == address) {
                    executor.child = None;
                }
            }
        });
        let mut descendants = Vec::new();
        let mut next = state.child.take();
        while let Some(task) = next {
            // SAFETY: every registered child is retained by its parent future.
            next = unsafe { (task.child)(task.data) };
            descendants.push(task);
        }
        for task in descendants.into_iter().rev() {
            // SAFETY: cancel child futures before parent locals are destroyed.
            // This preserves borrowed-resource destructor lifetimes as well as
            // keeping the native drop stack bounded. Ancestor boxes stay live.
            unsafe { (task.cancel)(task.data) };
        }
        // Ordinary field destruction now sees only cancelled child futures.
    }
}
/// Drive a recursive compiler computation with constant native poll depth.
pub(crate) fn run<T>(mut root: Continuation<'_, T>) -> T {
    let mut executor = Executor { child: None };
    struct Restore(*mut Executor);
    impl Drop for Restore {
        fn drop(&mut self) {
            EXECUTOR.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(EXECUTOR.with(|slot| slot.replace(&mut executor)));
    let state = root.state.as_mut().unwrap();
    let mut work = vec![Task {
        data: (&mut **state as *mut State<'_, T>).cast(),
        poll: poll_state::<T>,
        child: child_state::<T>,
        cancel: cancel_state::<T>,
    }];
    let mut context = Context::from_waker(Waker::noop());
    while let Some(task) = work.pop() {
        executor.child = None;
        // SAFETY: work contains the root and children registered by retained
        // ancestors. Each child finishes before its ancestor is polled again.
        match unsafe { (task.poll)(task.data, &mut context) } {
            Poll::Ready(()) => {
                assert!(executor.child.is_none());
            }
            Poll::Pending => {
                let child = executor
                    .child
                    .take()
                    .expect("compiler future yielded without a child");
                work.push(task);
                work.push(child);
            }
        }
    }
    root.state
        .as_mut()
        .unwrap()
        .result
        .take()
        .expect("compiler continuation result")
}

#[cfg(test)]
fn continuation<'a, T>(future: impl Future<Output = T> + 'a) -> Continuation<'a, T> {
    // SAFETY: these fixtures await directly owned children. The temporary-
    // child fixture unregisters its dropped child before returning Pending;
    // it deliberately tests rejection of a yield with no registered child.
    unsafe { Continuation::new(future) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descend<'a>(depth: usize, visits: &'a mut Vec<usize>) -> Continuation<'a, usize> {
        continuation(async move {
            visits.push(depth);
            let result = if depth == 0 {
                0
            } else {
                descend(depth - 1, visits).await + 1
            };
            visits.push(depth);
            result
        })
    }
    #[test]
    fn fifty_thousand_borrowing_continuations_on_one_mib() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let mut visits = Vec::new();
                assert_eq!(run(descend(50_000, &mut visits)), 50_000);
                assert_eq!(visits.len(), 100_002);
                assert_eq!(visits[0], 50_000);
                assert_eq!(visits[50_000], 0);
                assert_eq!(visits[100_001], 50_000);
            })
            .unwrap()
            .join()
            .unwrap();
    }
    fn fail(depth: usize) -> Continuation<'static, ()> {
        continuation(async move {
            if depth == 0 {
                panic!("leaf panic")
            } else {
                fail(depth - 1).await
            }
        })
    }
    #[test]
    fn panic_drops_fifty_thousand_suspended_frames_iteratively() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                assert!(std::panic::catch_unwind(|| run(fail(50_000))).is_err());
                assert_eq!(run(descend(2, &mut Vec::new())), 2);
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

#[cfg(test)]
mod safety_regression_tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn dropping_a_polled_temporary_child_cannot_leave_a_dangling_task() {
        let result = std::panic::catch_unwind(|| {
            run(continuation(std::future::poll_fn(|context| {
                let mut child = continuation(async { 7 });
                assert!(Pin::new(&mut child).poll(context).is_pending());
                drop(child);
                Poll::<()>::Pending
            })))
        });
        assert!(result.is_err());
        assert_eq!(run(continuation(async { 42 })), 42);
    }

    #[test]
    fn nested_run_restores_the_outer_executor() {
        let result = run(continuation(async {
            let inner = run(continuation(async { continuation(async { 20 }).await }));
            inner + continuation(async { 22 }).await
        }));
        assert_eq!(result, 42);
    }

    struct Owner {
        alive: Cell<bool>,
        events: Rc<RefCell<Vec<&'static str>>>,
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            self.alive.set(false);
            self.events.borrow_mut().push("owner");
        }
    }
    struct Borrowed<'a>(&'a Owner);
    impl Drop for Borrowed<'_> {
        fn drop(&mut self) {
            assert!(self.0.alive.get(), "borrowed destructor ran after owner");
            self.0.events.borrow_mut().push("borrow");
        }
    }

    #[test]
    fn cancellation_drops_borrowing_child_before_parent_local() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&events);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(continuation(async move {
                let owner = Owner {
                    alive: Cell::new(true),
                    events,
                };
                continuation(async {
                    let borrowed = Borrowed(&owner);
                    continuation(async { panic!("cancel active descendants") }).await;
                    drop(borrowed);
                })
                .await;
            }));
        }));
        assert!(result.is_err());
        assert_eq!(*observed.borrow(), ["borrow", "owner"]);
    }

    #[test]
    fn completed_child_is_consumed_before_parent_drop() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&events);
        run(continuation(async move {
            let owner = Owner {
                alive: Cell::new(true),
                events,
            };
            continuation(async {
                let borrowed = Borrowed(&owner);
                continuation(async {}).await;
                drop(borrowed);
            })
            .await;
        }));
        assert_eq!(*observed.borrow(), ["borrow", "owner"]);
    }
}

/// Incremental collection for callbacks that suspend between input elements.
/// Unlike buffering callback outputs before `collect`, the collector signals
/// the exact point at which `Option`/`Result` stop consuming their iterator.
pub(crate) trait ContinuationCollection<T>: Sized {
    type State: Default;
    fn push(state: &mut Self::State, value: T) -> std::ops::ControlFlow<()>;
    fn finish(state: Self::State) -> Self;
}

pub(crate) struct Collector<C: ContinuationCollection<T>, T> {
    state: C::State,
    _types: std::marker::PhantomData<fn(T) -> C>,
}
impl<C: ContinuationCollection<T>, T> Collector<C, T> {
    pub(crate) fn new() -> Self {
        Self {
            state: Default::default(),
            _types: std::marker::PhantomData,
        }
    }
    pub(crate) fn push(&mut self, value: T) -> std::ops::ControlFlow<()> {
        C::push(&mut self.state, value)
    }
    pub(crate) fn finish(self) -> C {
        C::finish(self.state)
    }
}

macro_rules! ordinary_collection {
    ($container:ty, $item:ty, [$($bound:tt)*]) => {
        impl<$($bound)*> ContinuationCollection<$item> for $container {
            type State = Self;
            fn push(state: &mut Self, value: $item) -> std::ops::ControlFlow<()> {
                state.extend(std::iter::once(value));
                std::ops::ControlFlow::Continue(())
            }
            fn finish(state: Self) -> Self { state }
        }
    };
}
ordinary_collection!(Vec<T>, T, [T]);
ordinary_collection!(std::collections::VecDeque<T>, T, [T]);
ordinary_collection!(std::collections::HashMap<K, V, S>, (K, V), [K: Eq + std::hash::Hash, V, S: std::hash::BuildHasher + Default]);
ordinary_collection!(std::collections::HashSet<T, S>, T, [T: Eq + std::hash::Hash, S: std::hash::BuildHasher + Default]);
ordinary_collection!(std::collections::BTreeMap<K, V>, (K, V), [K: Ord, V]);
ordinary_collection!(std::collections::BTreeSet<T>, T, [T: Ord]);
impl ContinuationCollection<char> for String {
    type State = String;
    fn push(state: &mut Self, value: char) -> std::ops::ControlFlow<()> {
        state.push(value);
        std::ops::ControlFlow::Continue(())
    }
    fn finish(state: Self) -> Self {
        state
    }
}

pub(crate) struct OptionalCollection<S> {
    value: Option<S>,
}
impl<S: Default> Default for OptionalCollection<S> {
    fn default() -> Self {
        Self {
            value: Some(S::default()),
        }
    }
}
impl<C: ContinuationCollection<T>, T> ContinuationCollection<Option<T>> for Option<C> {
    type State = OptionalCollection<C::State>;
    fn push(state: &mut Self::State, value: Option<T>) -> std::ops::ControlFlow<()> {
        match (state.value.as_mut(), value) {
            (Some(inner), Some(value)) => C::push(inner, value),
            (_, None) => {
                state.value = None;
                std::ops::ControlFlow::Break(())
            }
            (None, _) => std::ops::ControlFlow::Break(()),
        }
    }
    fn finish(state: Self::State) -> Self {
        state.value.map(C::finish)
    }
}

pub(crate) struct ResultCollection<S, E> {
    value: Option<S>,
    error: Option<E>,
}
impl<S: Default, E> Default for ResultCollection<S, E> {
    fn default() -> Self {
        Self {
            value: Some(S::default()),
            error: None,
        }
    }
}
impl<C: ContinuationCollection<T>, T, E> ContinuationCollection<Result<T, E>> for Result<C, E> {
    type State = ResultCollection<C::State, E>;
    fn push(state: &mut Self::State, value: Result<T, E>) -> std::ops::ControlFlow<()> {
        if state.error.is_some() {
            return std::ops::ControlFlow::Break(());
        }
        match value {
            Ok(value) => C::push(state.value.as_mut().expect("active collector"), value),
            Err(error) => {
                state.error = Some(error);
                state.value = None;
                std::ops::ControlFlow::Break(())
            }
        }
    }
    fn finish(state: Self::State) -> Self {
        match state.error {
            Some(error) => Err(error),
            None => Ok(C::finish(state.value.expect("successful collector"))),
        }
    }
}

#[cfg(test)]
#[allow(clippy::overly_complex_bool_expr, clippy::unnecessary_literal_unwrap)]
mod macro_semantic_tests {
    use super::*;
    #[derive(Default)]
    struct Probe {
        seen: Vec<i32>,
    }
    #[willow_continuations::methods(step, checked, optional, recurse, scenario, free_sum)]
    impl Probe {
        fn step(&mut self, value: i32) -> i32 {
            self.seen.push(value);
            value * 2
        }
        fn checked(&mut self, value: i32) -> Result<i32, &'static str> {
            let value = self.step(value);
            if value < 0 {
                Err("negative")
            } else {
                Ok(value)
            }
        }
        fn optional(&mut self, value: i32) -> Option<i32> {
            self.checked(value).ok()
        }
        fn recurse(&mut self, depth: i32, total: &mut i32) -> i32 {
            if depth == 0 {
                return 0;
            }
            let before = *total;
            *total += 1;
            let child = self.recurse(depth - 1, total);
            assert!(*total > before);
            child + 1
        }
        fn scenario(&mut self, mode: usize) -> i32 {
            match mode {
                0 => {
                    let before = self.step(2);
                    before + self.step(3)
                }
                1 => {
                    let mut count = 0;
                    self.recurse(8, &mut count) + count
                }
                2 => i32::from(false && self.step(1) > 0),
                3 => i32::from(true || self.step(1) > 0),
                4 => i32::from([1, 2, 3].iter().all(|v| self.step(*v) < 4)),
                5 => i32::from([1, 2, 3].iter().any(|v| self.step(*v) > 2)),
                6 => None::<i32>.map(|v| self.step(v)).unwrap_or(0),
                7 => Some(3).map(|v| self.step(v)).unwrap(),
                8 => i32::from(Some(3).is_some_and(|v| self.step(v) == 6)),
                9 => i32::from(None::<i32>.is_some_and(|v| self.step(v) == 6)),
                10 => i32::from(None::<i32>.is_none_or(|v| self.step(v) == 6)),
                11 => i32::from(Some(2).is_none_or(|v| self.step(v) == 4)),
                12 => {
                    let values: Vec<_> = [3, 1, 2].iter().map(|v| self.step(*v)).collect();
                    values.iter().sum()
                }
                13 => {
                    Some(2)
                        .map(|v| {
                            if self.step(v) == 4 {
                                return 7;
                            }
                            9
                        })
                        .unwrap()
                        + self.step(3)
                }
                14 => {
                    let value: Option<Result<i32, &'static str>> = Some(3).map(|v| {
                        let v = self.checked(v)?;
                        Ok(v + 1)
                    });
                    value.unwrap().unwrap()
                }
                15 => {
                    let result = [1, -1, 2]
                        .iter()
                        .map(|v| self.checked(*v))
                        .collect::<Result<Vec<_>, _>>();
                    assert_eq!(result, Err("negative"));
                    0
                }
                16 => {
                    let result = [1, -1, 2]
                        .iter()
                        .map(|v| self.optional(*v))
                        .collect::<Option<Vec<_>>>();
                    assert!(result.is_none());
                    0
                }
                17 => i32::from([].iter().all(|v| self.step(*v) > 0)),
                18 => i32::from([].iter().any(|v| self.step(*v) > 0)),
                19 => {
                    let result: Vec<_> = [(1, 2), (3, 4)]
                        .into_iter()
                        .map(|(a, b)| self.step(a) + b)
                        .collect();
                    result.iter().sum()
                }
                20 => {
                    let result: Option<Result<i32, &'static str>> = Some(-1).map(|v| {
                        let v = self.checked(v)?;
                        Ok(v + 1)
                    });
                    assert_eq!(result, Some(Err("negative")));
                    0
                }
                21 => free_sum(10),
                22 => {
                    let mut value = 4;
                    let borrowed = &mut value;
                    *borrowed += self.step(1);
                    value
                }
                23 => run(continuation(async { 11 })) + self.step(5),
                _ => unreachable!(),
            }
        }
    }
    #[willow_continuations::function(free_sum)]
    fn free_sum(value: i32) -> i32 {
        if value == 0 {
            return 0;
        }
        let next = value - 1;
        free_sum(next) + value
    }
    #[test]
    fn twenty_four_continuation_macro_semantic_perspectives() {
        let cases: [(&str, i32, &[i32]); 24] = [
            ("locals survive child", 10, &[2, 3]),
            ("recursive mutable borrow", 16, &[]),
            ("boolean and short circuit", 0, &[]),
            ("boolean or short circuit", 1, &[]),
            ("all stops on false", 0, &[1, 2]),
            ("any stops on true", 1, &[1, 2]),
            ("Option map None skips callback", 0, &[]),
            ("Option map Some", 6, &[3]),
            ("is_some_and Some", 1, &[3]),
            ("is_some_and None", 0, &[]),
            ("is_none_or None", 1, &[]),
            ("is_none_or Some", 1, &[2]),
            ("collect source ordering", 12, &[3, 1, 2]),
            ("closure return stays local", 13, &[2, 3]),
            ("closure question mark success", 7, &[3]),
            ("Result collect short circuit", 0, &[1, -1]),
            ("Option collect short circuit", 0, &[1, -1]),
            ("empty all", 1, &[]),
            ("empty any", 0, &[]),
            ("destructuring callback", 14, &[1, 3]),
            ("closure question mark error stays local", 0, &[-1]),
            ("free function recursion", 55, &[]),
            ("local mutable borrow across child", 6, &[1]),
            ("nested sync run", 21, &[5]),
        ];
        for (mode, (perspective, result, visits)) in cases.into_iter().enumerate() {
            let mut probe = Probe::default();
            assert_eq!(probe.scenario(mode), result, "{perspective}");
            assert_eq!(probe.seen, visits, "{perspective}");
        }
    }

    fn collect<C: ContinuationCollection<T>, T>(items: impl IntoIterator<Item = T>) -> C {
        let mut collector = Collector::new();
        for item in items {
            if collector.push(item).is_break() {
                break;
            }
        }
        collector.finish()
    }
    #[test]
    fn incremental_collectors_preserve_container_and_nested_short_circuit_semantics() {
        use std::collections::*;
        assert_eq!(collect::<Vec<_>, _>([1, 2, 1]), vec![1, 2, 1]);
        assert_eq!(
            collect::<VecDeque<_>, _>([1, 2, 1]),
            VecDeque::from([1, 2, 1])
        );
        assert_eq!(collect::<HashSet<_>, _>([1, 2, 1]), HashSet::from([1, 2]));
        assert_eq!(collect::<BTreeSet<_>, _>([2, 1, 1]), BTreeSet::from([1, 2]));
        assert_eq!(
            collect::<HashMap<_, _>, _>([(1, 2), (1, 3)]),
            HashMap::from([(1, 3)])
        );
        assert_eq!(
            collect::<BTreeMap<_, _>, _>([(1, 2), (1, 3)]),
            BTreeMap::from([(1, 3)])
        );
        assert_eq!(collect::<String, _>(['a', 'b']), "ab");
        let mut consumed = 0;
        let result: Option<Result<Vec<_>, &str>> = collect(
            [Some(Ok(1)), Some(Err("stop")), None]
                .into_iter()
                .inspect(|_| consumed += 1),
        );
        assert_eq!(result, Some(Err("stop")));
        assert_eq!(consumed, 2);
        consumed = 0;
        let result: Result<Option<Vec<_>>, &str> = collect(
            [Ok(Some(1)), Ok(None), Err("later")]
                .into_iter()
                .inspect(|_| consumed += 1),
        );
        assert_eq!(result, Ok(None));
        assert_eq!(consumed, 2);
    }
}
