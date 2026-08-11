//! Blocking compatibility cells (willow-dgwo.3, willow-38w.1.4/.1.5).
//!
//! `BlockingCell<T>` and `BlockingRwCell<T>` preserve the old single-operation
//! API after public `Mutex<T>`/`RwLock<T>` became scheduler-aware lexical
//! locks. Each is an opaque, program-lifetime (`Box::into_raw`) cell
//! holding the inner value as a single 64-bit word (scalars by value, GC values
//! as their pointer; the compiler coerces). A real `std::sync` lock guards the
//! word, so these explicit compatibility types may block a worker and should
//! not be confused with scheduler-aware locks.
//!
//! GC: a cell whose element type is a reference holds a live root. Because the
//! compatibility cells are leaked, ref cells are recorded in a registry and
//! their current word is reported to the collector.

use std::os::raw::c_void;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock as StdRwLock;

struct WillowBlockingCell {
    value: StdMutex<i64>,
    is_ref: bool,
}

struct WillowBlockingRwCell {
    value: StdRwLock<i64>,
    is_ref: bool,
}

/// Registries of ref-holding locks so the collector can trace the held value.
/// Locks are program-lifetime (leaked), so entries are never removed.
static BLOCKING_CELL_GC_REGISTRY: StdMutex<Vec<usize>> = StdMutex::new(Vec::new());
static BLOCKING_RW_CELL_GC_REGISTRY: StdMutex<Vec<usize>> = StdMutex::new(Vec::new());

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_cell_new(value: i64, is_ref: i64) -> *mut c_void {
    let is_ref = is_ref != 0;
    let raw = Box::into_raw(Box::new(WillowBlockingCell {
        value: StdMutex::new(value),
        is_ref,
    }));
    if is_ref {
        BLOCKING_CELL_GC_REGISTRY
            .lock()
            .expect("mutex registry poisoned")
            .push(raw as usize);
    }
    raw as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_cell_get(raw: *mut c_void) -> i64 {
    let m = unsafe { &*(raw as *const WillowBlockingCell) };
    *m.value.lock().expect("mutex poisoned")
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_cell_set(raw: *mut c_void, value: i64) {
    let m = unsafe { &*(raw as *const WillowBlockingCell) };
    *m.value.lock().expect("mutex poisoned") = value;
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_rw_cell_new(value: i64, is_ref: i64) -> *mut c_void {
    let is_ref = is_ref != 0;
    let raw = Box::into_raw(Box::new(WillowBlockingRwCell {
        value: StdRwLock::new(value),
        is_ref,
    }));
    if is_ref {
        BLOCKING_RW_CELL_GC_REGISTRY
            .lock()
            .expect("rwlock registry poisoned")
            .push(raw as usize);
    }
    raw as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_rw_cell_read(raw: *mut c_void) -> i64 {
    let r = unsafe { &*(raw as *const WillowBlockingRwCell) };
    *r.value.read().expect("rwlock poisoned")
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_rw_cell_write(raw: *mut c_void, value: i64) {
    let r = unsafe { &*(raw as *const WillowBlockingRwCell) };
    *r.value.write().expect("rwlock poisoned") = value;
}

/// Live GC roots held by ref-typed blocking compatibility cells: the current
/// word of each ref cell is a pointer the collector must keep alive.
pub(crate) fn lock_gc_roots() -> Vec<*mut u8> {
    let mut roots = Vec::new();
    if let Ok(reg) = BLOCKING_CELL_GC_REGISTRY.lock() {
        for &addr in reg.iter() {
            let m = unsafe { &*(addr as *const WillowBlockingCell) };
            if m.is_ref
                && let Ok(v) = m.value.lock()
            {
                let p = *v as *mut u8;
                if !p.is_null() {
                    roots.push(p);
                }
            }
        }
    }
    if let Ok(reg) = BLOCKING_RW_CELL_GC_REGISTRY.lock() {
        for &addr in reg.iter() {
            let r = unsafe { &*(addr as *const WillowBlockingRwCell) };
            if r.is_ref
                && let Ok(v) = r.value.read()
            {
                let p = *v as *mut u8;
                if !p.is_null() {
                    roots.push(p);
                }
            }
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutex_get_set() {
        let m = willow_blocking_cell_new(7, 0);
        assert_eq!(willow_blocking_cell_get(m), 7);
        willow_blocking_cell_set(m, 42);
        assert_eq!(willow_blocking_cell_get(m), 42);
    }

    #[test]
    fn rwlock_read_write() {
        let r = willow_blocking_rw_cell_new(1, 0);
        assert_eq!(willow_blocking_rw_cell_read(r), 1);
        willow_blocking_rw_cell_write(r, 100);
        assert_eq!(willow_blocking_rw_cell_read(r), 100);
    }
}
