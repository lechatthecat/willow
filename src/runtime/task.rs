use crate::runtime::stack_trace::RuntimeStackTrace;

pub type RuntimeTaskId = u64;
pub type GcRootSlot = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTaskState {
    Ready,
    Running,
    Parked,
    Completed,
    Panicked,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcRootSet {
    slots: Vec<GcRootSlot>,
}

impl GcRootSet {
    pub fn push(&mut self, slot: GcRootSlot) {
        self.slots.push(slot);
    }

    pub fn pop(&mut self) -> Option<GcRootSlot> {
        self.slots.pop()
    }

    pub fn slots(&self) -> &[GcRootSlot] {
        &self.slots
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeTask {
    pub id: RuntimeTaskId,
    pub state: RuntimeTaskState,
    pub roots: GcRootSet,
    pub spawned_from: Option<RuntimeStackTrace>,
    pub stack_trace: RuntimeStackTrace,
}

impl RuntimeTask {
    pub fn new(id: RuntimeTaskId) -> Self {
        Self {
            id,
            state: RuntimeTaskState::Ready,
            roots: GcRootSet::default(),
            spawned_from: None,
            stack_trace: RuntimeStackTrace::default(),
        }
    }

    pub fn park(&mut self) {
        self.state = RuntimeTaskState::Parked;
    }

    pub fn wake(&mut self) {
        if self.state == RuntimeTaskState::Parked {
            self.state = RuntimeTaskState::Ready;
        }
    }

    pub fn complete(&mut self) {
        self.state = RuntimeTaskState::Completed;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinHandle<T> {
    task_id: RuntimeTaskId,
    result: Option<T>,
}

impl<T> JoinHandle<T> {
    pub fn pending(task_id: RuntimeTaskId) -> Self {
        Self {
            task_id,
            result: None,
        }
    }

    pub fn complete(task_id: RuntimeTaskId, result: T) -> Self {
        Self {
            task_id,
            result: Some(result),
        }
    }

    pub fn task_id(&self) -> RuntimeTaskId {
        self.task_id
    }

    pub fn join(self) -> Option<T> {
        self.result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_set_preserves_slots() {
        let mut roots = GcRootSet::default();
        roots.push(10);
        roots.push(20);
        assert_eq!(roots.slots(), &[10, 20]);
        assert_eq!(roots.pop(), Some(20));
    }

    #[test]
    fn task_state_transitions_are_explicit() {
        let mut task = RuntimeTask::new(7);
        task.park();
        assert_eq!(task.state, RuntimeTaskState::Parked);
        task.wake();
        assert_eq!(task.state, RuntimeTaskState::Ready);
        task.complete();
        assert_eq!(task.state, RuntimeTaskState::Completed);
    }
}
