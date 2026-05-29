use crate::trace::{GcRootSet, GcTrace, GcVisitor};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll<T> {
    Ready(T),
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeFutureState<T> {
    Pending,
    Ready(T),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeFuture<T> {
    state: RuntimeFutureState<T>,
    roots: GcRootSet,
}

impl<T: Clone> RuntimeFuture<T> {
    pub fn pending() -> Self {
        Self {
            state: RuntimeFutureState::Pending,
            roots: GcRootSet::default(),
        }
    }

    pub fn complete(&mut self, value: T) {
        self.state = RuntimeFutureState::Ready(value);
    }

    pub fn cancel(&mut self) {
        self.state = RuntimeFutureState::Cancelled;
    }

    pub fn poll(&self) -> Poll<T> {
        match &self.state {
            RuntimeFutureState::Ready(value) => Poll::Ready(value.clone()),
            RuntimeFutureState::Pending | RuntimeFutureState::Cancelled => Poll::Pending,
        }
    }

    pub fn roots(&self) -> &GcRootSet {
        &self.roots
    }

    pub fn roots_mut(&mut self) -> &mut GcRootSet {
        &mut self.roots
    }
}

impl<T: GcTrace> GcTrace for RuntimeFuture<T> {
    fn trace(&self, visitor: &mut GcVisitor) {
        self.roots.trace(visitor);
        if let RuntimeFutureState::Ready(value) = &self.state {
            value.trace(visitor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TestRoot(usize);

    impl GcTrace for TestRoot {
        fn trace(&self, visitor: &mut GcVisitor) {
            visitor.mark_root(self.0);
        }
    }

    #[test]
    fn future_moves_from_pending_to_ready() {
        let mut future = RuntimeFuture::pending();
        assert_eq!(future.poll(), Poll::Pending);
        future.complete(42);
        assert_eq!(future.poll(), Poll::Ready(42));
    }

    #[test]
    fn future_traces_roots_and_ready_result() {
        let mut future = RuntimeFuture::pending();
        future.roots_mut().push(11);
        future.complete(TestRoot(22));

        let mut visitor = GcVisitor::default();
        future.trace(&mut visitor);

        assert_eq!(visitor.roots(), &[11, 22]);
    }
}
