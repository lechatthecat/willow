//! A Rust-owned event source with one consumer and an explicit wakeup.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
#[derive(Default)]
struct State {
    values: VecDeque<String>,
    waiter: Option<Waker>,
    closed: bool,
}
pub struct Source(Arc<Mutex<State>>);
impl Default for Source {
    fn default() -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let producer = state.clone();
        std::thread::spawn(move || {
            for i in 0..3 {
                std::thread::sleep(std::time::Duration::from_millis(10));
                let wake = {
                    let mut state = producer.lock().unwrap();
                    state.values.push_back(format!("request-{i}"));
                    state.waiter.take()
                };
                if let Some(waker) = wake {
                    waker.wake();
                }
            }
            let wake = {
                let mut state = producer.lock().unwrap();
                state.closed = true;
                state.waiter.take()
            };
            if let Some(waker) = wake {
                waker.wake();
            }
        });
        Self(state)
    }
}
impl Source {
    pub async fn next(&self) -> Option<String> {
        std::future::poll_fn(|cx| {
            let mut state = self.0.lock().unwrap();
            if let Some(value) = state.values.pop_front() {
                return Poll::Ready(Some(value));
            }
            if state.closed {
                return Poll::Ready(None);
            }
            state.waiter = Some(cx.waker().clone());
            Poll::Pending
        })
        .await
    }
}
