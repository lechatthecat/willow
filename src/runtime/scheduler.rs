use std::collections::{HashMap, VecDeque};

use crate::runtime::task::{RuntimeTask, RuntimeTaskId, RuntimeTaskState};

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

    pub fn pop_ready(&mut self) -> Option<RuntimeTaskId> {
        self.ready.pop_front()
    }

    pub fn task(&self, id: RuntimeTaskId) -> Option<&RuntimeTask> {
        self.tasks.get(&id)
    }

    pub fn task_mut(&mut self, id: RuntimeTaskId) -> Option<&mut RuntimeTask> {
        self.tasks.get_mut(&id)
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
}
