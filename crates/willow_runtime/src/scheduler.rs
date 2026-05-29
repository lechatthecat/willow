use std::collections::{HashMap, VecDeque};

use crate::task::{RuntimeTask, RuntimeTaskId, RuntimeTaskState};
use crate::trace::{GcTrace, GcVisitor};

#[derive(Debug, Default)]
pub struct RuntimeScheduler {
    next_task_id: RuntimeTaskId,
    tasks: HashMap<RuntimeTaskId, RuntimeTask>,
    ready: VecDeque<RuntimeTaskId>,
}

impl RuntimeScheduler {
    pub fn spawn_placeholder(&mut self) -> RuntimeTaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        let task = RuntimeTask::new(id);
        self.tasks.insert(id, task);
        self.ready.push_back(id);
        id
    }

    pub fn spawn_parked_placeholder(&mut self) -> RuntimeTaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        let mut task = RuntimeTask::new(id);
        task.park();
        self.tasks.insert(id, task);
        id
    }

    pub fn pop_ready(&mut self) -> Option<RuntimeTaskId> {
        self.ready.pop_front()
    }

    pub fn task(&self, id: RuntimeTaskId) -> Option<&RuntimeTask> {
        self.tasks.get(&id)
    }

    pub fn task_mut(&mut self, id: RuntimeTaskId) -> Option<&mut RuntimeTask> {
        self.tasks.get_mut(&id)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &RuntimeTask> {
        self.tasks.values()
    }

    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }

    pub fn ready_len(&self) -> usize {
        self.ready.len()
    }

    pub fn task_state(&self, id: RuntimeTaskId) -> Option<RuntimeTaskState> {
        self.tasks.get(&id).map(|task| task.state)
    }

    pub fn park(&mut self, id: RuntimeTaskId) {
        if let Some(task) = self.tasks.get_mut(&id) {
            task.park();
        }
    }

    pub fn wake(&mut self, id: RuntimeTaskId) {
        if let Some(task) = self.tasks.get_mut(&id) {
            let was_parked = task.state == RuntimeTaskState::Parked;
            task.wake();
            if was_parked {
                self.ready.push_back(id);
            }
        }
    }
}

impl GcTrace for RuntimeScheduler {
    fn trace(&self, visitor: &mut GcVisitor) {
        for task in self.tasks.values() {
            task.trace(visitor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_assigns_ready_task_ids() {
        let mut scheduler = RuntimeScheduler::default();
        let first = scheduler.spawn_placeholder();
        let second = scheduler.spawn_placeholder();
        assert_eq!(scheduler.pop_ready(), Some(first));
        assert_eq!(scheduler.pop_ready(), Some(second));
    }

    #[test]
    fn scheduler_traces_all_task_roots() {
        let mut scheduler = RuntimeScheduler::default();
        let first = scheduler.spawn_placeholder();
        let second = scheduler.spawn_placeholder();
        scheduler.task_mut(first).unwrap().roots.push(10);
        scheduler.task_mut(second).unwrap().roots.push(20);

        let mut visitor = GcVisitor::default();
        scheduler.trace(&mut visitor);
        let mut roots = visitor.into_roots();
        roots.sort_unstable();

        assert_eq!(roots, vec![10, 20]);
    }

    #[test]
    fn scheduler_reports_task_and_ready_counts() {
        let mut scheduler = RuntimeScheduler::default();
        assert_eq!(scheduler.task_count(), 0);
        assert_eq!(scheduler.ready_len(), 0);
        scheduler.spawn_placeholder();
        scheduler.spawn_placeholder();
        assert_eq!(scheduler.task_count(), 2);
        assert_eq!(scheduler.ready_len(), 2);
    }

    #[test]
    fn scheduler_park_removes_task_from_running_state_only() {
        let mut scheduler = RuntimeScheduler::default();
        let id = scheduler.spawn_placeholder();
        scheduler.park(id);
        assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Parked));
    }

    #[test]
    fn scheduler_wake_requeues_parked_task() {
        let mut scheduler = RuntimeScheduler::default();
        let id = scheduler.spawn_placeholder();
        assert_eq!(scheduler.pop_ready(), Some(id));
        scheduler.park(id);
        scheduler.wake(id);
        assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Ready));
        assert_eq!(scheduler.pop_ready(), Some(id));
    }

    #[test]
    fn scheduler_wake_ready_task_does_not_duplicate_ready_queue() {
        let mut scheduler = RuntimeScheduler::default();
        let id = scheduler.spawn_placeholder();
        scheduler.wake(id);
        assert_eq!(scheduler.ready_len(), 1);
        assert_eq!(scheduler.pop_ready(), Some(id));
        assert_eq!(scheduler.pop_ready(), None);
    }

    #[test]
    fn scheduler_spawn_parked_placeholder_does_not_enter_ready_queue() {
        let mut scheduler = RuntimeScheduler::default();
        let id = scheduler.spawn_parked_placeholder();
        assert_eq!(scheduler.ready_len(), 0);
        assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Parked));
    }
}
