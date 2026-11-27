use std::sync::{OnceLock, mpsc};
use std::thread::{self, ThreadId};
use std::time::Duration;

unsafe extern "C" {
    fn willow_gc_register_mutator();
    fn willow_sched_run() -> i64;
    fn willow_blocking_active_jobs() -> i64;
    fn willow_blocking_queued_jobs() -> i64;
    fn willow_blocking_completed_jobs() -> i64;
}

static CALLER: OnceLock<ThreadId> = OnceLock::new();

pub fn remember_thread() {
    CALLER.set(thread::current().id()).unwrap();
    // This verification fixture registers synchronous main before the calls
    // under test, so the other thread's collection must account for its roots.
    unsafe { willow_gc_register_mutator() };
}

pub fn unknown(text: &str) -> String {
    wait_for_tasks(text)
}

pub fn blocking(text: &str) -> String {
    wait_for_tasks(text)
}

fn native_callback(stack: &mut [u8; 65536], expected: *const u8) {
    assert_eq!(thread::current().id(), *CALLER.get().unwrap());
    assert_eq!(stack.as_ptr(), expected);
    assert_eq!(stack[1234], 42);
}

fn wait_for_tasks(text: &str) -> String {
    let mut stack = [42u8; 65536];
    let address = stack.as_ptr();
    native_callback(&mut stack, address);
    let borrowed = text.as_ptr();
    let (done, completed) = mpsc::channel();
    // Explicitly drive already queued Willow Tasks on an independent thread.
    // The caller remains blocked in its generated adapter's GC safe region.
    let driver = thread::spawn(move || {
        unsafe { willow_sched_run() };
        done.send(()).unwrap();
    });
    completed
        .recv_timeout(Duration::from_secs(30))
        .expect("foreign call stalled Tasks or GC");
    driver.join().unwrap();
    native_callback(&mut stack, address);
    assert_eq!(text.as_ptr(), borrowed);
    assert_eq!(unsafe { willow_blocking_active_jobs() }, 0);
    assert_eq!(unsafe { willow_blocking_queued_jobs() }, 0);
    assert_eq!(unsafe { willow_blocking_completed_jobs() }, 0);
    text.to_owned()
}
