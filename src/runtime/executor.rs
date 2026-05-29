use crate::runtime::scheduler::RuntimeScheduler;
use crate::runtime::task::{RuntimeTaskId, RuntimeTaskState};

#[derive(Debug, Default)]
pub struct RuntimeExecutor {
    scheduler: RuntimeScheduler,
}

impl RuntimeExecutor {
    pub fn scheduler(&self) -> &RuntimeScheduler {
        &self.scheduler
    }

    pub fn scheduler_mut(&mut self) -> &mut RuntimeScheduler {
        &mut self.scheduler
    }

    pub fn spawn_placeholder(&mut self) -> RuntimeTaskId {
        self.scheduler.spawn_placeholder()
    }

    pub fn run_until_idle(&mut self) -> usize {
        let mut completed = 0;
        while let Some(task_id) = self.scheduler.pop_ready() {
            if let Some(task) = self.scheduler.task_mut(task_id) {
                task.state = RuntimeTaskState::Running;
                task.complete();
                completed += 1;
            }
        }
        completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_drains_ready_tasks() {
        let mut executor = RuntimeExecutor::default();
        executor.spawn_placeholder();
        executor.spawn_placeholder();
        assert_eq!(executor.run_until_idle(), 2);
    }
}
