use crate::task::RuntimeTaskId;
use crate::trace::{GcTrace, GcVisitor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoInterest {
    Readable,
    Writable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoRegistration {
    pub token: usize,
    pub task_id: RuntimeTaskId,
    pub interest: IoInterest,
}

#[derive(Debug, Default)]
pub struct RuntimeNetPoll {
    registrations: Vec<IoRegistration>,
}

impl RuntimeNetPoll {
    pub fn register(&mut self, registration: IoRegistration) {
        self.registrations.push(registration);
    }

    pub fn registrations(&self) -> &[IoRegistration] {
        &self.registrations
    }

    pub fn ready_tasks(&self, token: usize) -> Vec<RuntimeTaskId> {
        self.registrations
            .iter()
            .filter(|registration| registration.token == token)
            .map(|registration| registration.task_id)
            .collect()
    }
}

impl GcTrace for RuntimeNetPoll {
    fn trace(&self, _visitor: &mut GcVisitor) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netpoll_maps_tokens_to_tasks() {
        let mut poll = RuntimeNetPoll::default();
        poll.register(IoRegistration {
            token: 3,
            task_id: 9,
            interest: IoInterest::Readable,
        });
        assert_eq!(poll.ready_tasks(3), vec![9]);
    }
}
