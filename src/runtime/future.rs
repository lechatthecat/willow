use crate::runtime::task::GcRootSet;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn future_moves_from_pending_to_ready() {
        let mut future = RuntimeFuture::pending();
        assert_eq!(future.poll(), Poll::Pending);
        future.complete(42);
        assert_eq!(future.poll(), Poll::Ready(42));
    }
}
