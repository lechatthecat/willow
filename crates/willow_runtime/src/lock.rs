//! `Mutex<T>` and `RwLock<T>` synchronization primitives (willow-dgwo.3).
//!
//! Each is an opaque, program-lifetime (`Box::into_raw`, like channels) cell
//! holding the inner value as a single 64-bit word (scalars by value, GC values
//! as their pointer; the compiler coerces). A real `std::sync` lock guards the
//! word so the primitives are correct when `WILLOW_WORKERS=N` enables
//! multi-worker execution; default single-worker runs usually leave the lock
//! uncontended.
//!
//! GC: a cell whose element type is a reference holds a live root. Because cells
//! are leaked, ref cells are recorded in a registry and their current word is
//! reported to the collector (mirrors the channel GC-root scheme).

use std::os::raw::c_void;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock as StdRwLock;

struct WillowMutex {
    value: StdMutex<i64>,
    is_ref: bool,
}

struct WillowRwLock {
    value: StdRwLock<i64>,
    is_ref: bool,
}

/// Registries of ref-holding locks so the collector can trace the held value.
/// Locks are program-lifetime (leaked), so entries are never removed.
static MUTEX_GC_REGISTRY: StdMutex<Vec<usize>> = StdMutex::new(Vec::new());
static RWLOCK_GC_REGISTRY: StdMutex<Vec<usize>> = StdMutex::new(Vec::new());

#[unsafe(no_mangle)]
pub extern "C" fn willow_mutex_new(value: i64, is_ref: i64) -> *mut c_void {
    let is_ref = is_ref != 0;
    let raw = Box::into_raw(Box::new(WillowMutex {
        value: StdMutex::new(value),
        is_ref,
    }));
    if is_ref {
        MUTEX_GC_REGISTRY
            .lock()
            .expect("mutex registry poisoned")
            .push(raw as usize);
    }
    raw as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_mutex_get(raw: *mut c_void) -> i64 {
    let m = unsafe { &*(raw as *const WillowMutex) };
    *m.value.lock().expect("mutex poisoned")
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_mutex_set(raw: *mut c_void, value: i64) {
    let m = unsafe { &*(raw as *const WillowMutex) };
    *m.value.lock().expect("mutex poisoned") = value;
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_rwlock_new(value: i64, is_ref: i64) -> *mut c_void {
    let is_ref = is_ref != 0;
    let raw = Box::into_raw(Box::new(WillowRwLock {
        value: StdRwLock::new(value),
        is_ref,
    }));
    if is_ref {
        RWLOCK_GC_REGISTRY
            .lock()
            .expect("rwlock registry poisoned")
            .push(raw as usize);
    }
    raw as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_rwlock_read(raw: *mut c_void) -> i64 {
    let r = unsafe { &*(raw as *const WillowRwLock) };
    *r.value.read().expect("rwlock poisoned")
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_rwlock_write(raw: *mut c_void, value: i64) {
    let r = unsafe { &*(raw as *const WillowRwLock) };
    *r.value.write().expect("rwlock poisoned") = value;
}

/// Live GC roots held by ref-typed `Mutex`/`RwLock` cells: the current word of
/// each ref cell is a pointer the collector must keep alive.
pub(crate) fn lock_gc_roots() -> Vec<*mut u8> {
    let mut roots = Vec::new();
    if let Ok(reg) = MUTEX_GC_REGISTRY.lock() {
        for &addr in reg.iter() {
            let m = unsafe { &*(addr as *const WillowMutex) };
            if m.is_ref {
                if let Ok(v) = m.value.lock() {
                    let p = *v as *mut u8;
                    if !p.is_null() {
                        roots.push(p);
                    }
                }
            }
        }
    }
    if let Ok(reg) = RWLOCK_GC_REGISTRY.lock() {
        for &addr in reg.iter() {
            let r = unsafe { &*(addr as *const WillowRwLock) };
            if r.is_ref {
                if let Ok(v) = r.value.read() {
                    let p = *v as *mut u8;
                    if !p.is_null() {
                        roots.push(p);
                    }
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
        let m = willow_mutex_new(7, 0);
        assert_eq!(willow_mutex_get(m), 7);
        willow_mutex_set(m, 42);
        assert_eq!(willow_mutex_get(m), 42);
    }

    #[test]
    fn rwlock_read_write() {
        let r = willow_rwlock_new(1, 0);
        assert_eq!(willow_rwlock_read(r), 1);
        willow_rwlock_write(r, 100);
        assert_eq!(willow_rwlock_read(r), 100);
    }
}
