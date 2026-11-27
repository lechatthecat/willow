use std::sync::atomic::{AtomicUsize, Ordering};
static DROPS: AtomicUsize = AtomicUsize::new(0);
pub struct Counter { value: i64 }
impl Counter {
    pub fn new(value: i64) -> Self { Self { value } }
    pub fn read(&self) -> i64 { self.value }
    pub fn finish(self) -> i64 { self.value }
}
impl Drop for Counter {
    fn drop(&mut self) { DROPS.fetch_add(1, Ordering::SeqCst); }
}
pub fn drops() -> i64 { DROPS.load(Ordering::SeqCst) as i64 }
