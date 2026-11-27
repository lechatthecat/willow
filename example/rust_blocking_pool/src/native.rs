//! Two ordinary synchronous calls rendezvous on the Rust blocking pool.
//! Run with WILLOW_WORKERS=1: neither call occupies that scheduler worker.
use std::sync::{Condvar, Mutex};
static ARRIVALS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());
pub fn meet(value: i64) -> i64 {
    let (lock, ready) = &ARRIVALS;
    let mut count = lock.lock().unwrap();
    *count += 1;
    ready.notify_all();
    while *count < 2 {
        let (next, timeout) = ready.wait_timeout(count, std::time::Duration::from_secs(30)).unwrap();
        count = next;
        assert!(!timeout.timed_out(), "second blocking call never started");
    }
    value
}
pub struct Resource;
pub fn resource() -> Box<Resource> { Box::new(Resource) }
pub fn value(_: &Resource) -> i64 { 42 }
