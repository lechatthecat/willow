use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelError {
    Closed,
    Empty,
}

#[derive(Debug, Clone)]
pub struct RuntimeChannel<T> {
    buffer: VecDeque<T>,
    closed: bool,
    element_type_id: i64,
}

impl<T> RuntimeChannel<T> {
    pub fn new(element_type_id: i64) -> Self {
        Self {
            buffer: VecDeque::new(),
            closed: false,
            element_type_id,
        }
    }

    pub fn element_type_id(&self) -> i64 {
        self.element_type_id
    }

    pub fn send(&mut self, value: T) -> Result<(), ChannelError> {
        if self.closed {
            return Err(ChannelError::Closed);
        }
        self.buffer.push_back(value);
        Ok(())
    }

    pub fn recv(&mut self) -> Result<T, ChannelError> {
        self.buffer.pop_front().ok_or(ChannelError::Empty)
    }

    pub fn close(&mut self) {
        self.closed = true;
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_buffers_values_and_closes() {
        let mut channel = RuntimeChannel::new(1);
        channel.send(10).unwrap();
        channel.send(20).unwrap();
        assert_eq!(channel.recv(), Ok(10));
        channel.close();
        assert_eq!(channel.send(30), Err(ChannelError::Closed));
        assert_eq!(channel.recv(), Ok(20));
        assert_eq!(channel.recv(), Err(ChannelError::Empty));
    }
}
