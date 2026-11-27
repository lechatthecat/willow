// Shared source compiled in the generated bridge so Rust owns every allocation.
// No native pointer is exported to Willow. Explicit close only; no GC finalizer.
use std::any::{Any, TypeId};
use std::sync::{Arc, Mutex, OnceLock};
pub type Lease = Arc<Box<dyn Any + Send + Sync>>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleError {
    Invalid,
    Closed,
    Borrowed,
}

struct Slot {
    generation: u32,
    name: TypeId,
    #[cfg(any(test, debug_assertions))]
    type_name: &'static str,
    value: Option<Lease>,
}
#[derive(Default)]
pub struct Table {
    slots: Vec<Slot>,
    free: Vec<u32>,
    probes: ProbeCount,
}
/// Slot-lookup probes observed by this file's scaling test. This source is
/// also compiled into generated bridges, so it carries its own erasure: the
/// count is zero-sized and recording is a no-op outside tests.
#[derive(Default)]
struct ProbeCount(#[cfg(test)] std::cell::Cell<usize>);
impl ProbeCount {
    #[cfg(not(test))]
    #[inline]
    fn record(&self) {}
    #[cfg(test)]
    fn record(&self) {
        self.0.set(self.0.get() + 1);
    }
    #[cfg(test)]
    fn get(&self) -> usize {
        self.0.get()
    }
    #[cfg(test)]
    fn set(&self, value: usize) {
        self.0.set(value);
    }
}
impl Table {
    pub fn insert<T: Any + Send + Sync>(&mut self, value: Box<T>, name: TypeId) -> u64 {
        let value: Box<dyn Any + Send + Sync> = value;
        let value = Some(Arc::new(value));
        let index = if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.generation += 1;
            slot.name = name;
            #[cfg(any(test, debug_assertions))]
            {
                slot.type_name = std::any::type_name::<T>();
            }
            slot.value = value;
            index
        } else {
            let index = u32::try_from(self.slots.len()).expect("Rust handle table exhausted");
            // Low word zero is reserved for invalid IDs.
            assert!(index < u32::MAX, "Rust handle table exhausted");
            self.slots.push(Slot {
                generation: 1,
                name,
                #[cfg(any(test, debug_assertions))]
                type_name: std::any::type_name::<T>(),
                value,
            });
            index
        };
        (u64::from(self.slots[index as usize].generation) << 32) | (u64::from(index) + 1)
    }
    fn index(&self, id: u64, name: TypeId) -> Result<usize, HandleError> {
        let index = (id as u32).checked_sub(1).ok_or(HandleError::Invalid)? as usize;
        self.probes.record();
        let slot = self.slots.get(index).ok_or(HandleError::Invalid)?;
        let generation = (id >> 32) as u32;
        if generation == 0 || generation > slot.generation {
            return Err(HandleError::Invalid);
        }
        if generation < slot.generation {
            return Err(HandleError::Closed);
        }
        if slot.name != name {
            return Err(HandleError::Invalid);
        }
        if slot.value.is_none() {
            return Err(HandleError::Closed);
        }
        Ok(index)
    }
    pub fn get(&self, id: u64, name: TypeId) -> Result<Lease, HandleError> {
        Ok(self.slots[self.index(id, name)?]
            .value
            .as_ref()
            .unwrap()
            .clone())
    }
    // Return ownership to the caller: arbitrary user destructors must never run
    // while the table mutex is locked (they may reenter the bridge).
    pub fn close(&mut self, id: u64, name: TypeId) -> Result<Lease, HandleError> {
        let index = self.index(id, name)?;
        Ok(self.remove(index))
    }
    fn take<T: Any + Send + Sync>(&mut self, id: u64, name: TypeId) -> Result<Lease, HandleError> {
        let index = self.index(id, name)?;
        let value = self.slots[index].value.as_mut().unwrap();
        if !value.is::<T>() {
            return Err(HandleError::Invalid);
        }
        // Check strong and weak exclusivity together. Once unique, the table
        // mutex prevents another lease from being issued before removal.
        if Arc::get_mut(value).is_none() {
            return Err(HandleError::Borrowed);
        }
        Ok(self.remove(index))
    }
    fn remove(&mut self, index: usize) -> Lease {
        let slot = &mut self.slots[index];
        let value = slot.value.take().unwrap();
        if slot.generation < u32::MAX {
            self.free.push(index as u32);
        }
        value
    }
}
fn table() -> &'static Mutex<Table> {
    static TABLE: OnceLock<Mutex<Table>> = OnceLock::new();
    TABLE.get_or_init(|| {
        // Reuse table initialization, with no additional per-operation guard.
        // Release bridges have neither type-name storage nor a reporter.
        #[cfg(any(test, debug_assertions))]
        // SAFETY: the bridge callback has process lifetime and never unwinds.
        unsafe {
            willow_rust_bridge_register_reporter(report_unclosed);
        }
        Mutex::new(Table::default())
    })
}

#[cfg(any(test, debug_assertions))]
unsafe extern "C" {
    fn willow_rust_bridge_register_reporter(report: extern "C" fn());
    fn willow_debug_report_handle_site(id: u64, name: *const u8, len: usize);
}

#[cfg(any(test, debug_assertions))]
impl Table {
    fn unclosed_counts(&self) -> Vec<(&'static str, usize)> {
        let mut counts = std::collections::HashMap::new();
        for slot in &self.slots {
            self.probes.record();
            if slot.value.is_some() {
                *counts.entry(slot.type_name).or_insert(0) += 1;
            }
        }
        let mut counts: Vec<_> = counts.into_iter().collect();
        counts.sort_unstable_by_key(|(name, _)| *name);
        counts
    }
}

#[cfg(any(test, debug_assertions))]
extern "C" fn report_unclosed() {
    use std::io::Write;
    // Never destroy handles or invoke user code. Release the table mutex
    // before writing; a closed stderr must not affect the program's status.
    let (counts, live) = {
        let table = table().lock().unwrap_or_else(|p| p.into_inner());
        let live: Vec<_> = table
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.value.as_ref().map(|_| {
                    (
                        (u64::from(slot.generation) << 32) | (index as u64 + 1),
                        slot.type_name,
                    )
                })
            })
            .collect();
        (table.unclosed_counts(), live)
    };
    let mut stderr = std::io::stderr().lock();
    for (name, count) in counts {
        let _ = writeln!(stderr, "Willow unclosed Rust handles: {name}: {count}");
    }
    drop(stderr);
    for (id, name) in live {
        // SAFETY: each type name is static UTF-8; IDs were snapshotted while locked.
        unsafe {
            willow_debug_report_handle_site(id, name.as_ptr(), name.len());
        }
    }
}
pub fn insert<T: Any + Send + Sync>(value: Box<T>, name: TypeId) -> u64 {
    table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(value, name)
}
pub fn get(id: u64, name: TypeId) -> Result<Lease, HandleError> {
    table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(id, name)
}
/// Consume an unborrowed handle, invalidating every copy of its generation ID.
/// Unwrapping and arbitrary user destruction happen after releasing the mutex.
pub fn take<T: Any + Send + Sync>(id: u64, name: TypeId) -> Result<Box<T>, HandleError> {
    let removed = table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take::<T>(id, name)?;
    let owned = Arc::try_unwrap(removed).map_err(|_| HandleError::Borrowed)?;
    owned.downcast::<T>().map_err(|_| HandleError::Invalid)
}

pub fn detach(id: u64, name: TypeId) -> Result<Lease, HandleError> {
    table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .close(id, name)
}
pub fn close(id: u64, name: TypeId) -> Result<(), HandleError> {
    let removed = detach(id, name)?;
    drop(removed);
    Ok(())
}

/// Scope cleanup is a no-op after explicit close or consumption. Invalid IDs
/// remain errors; only a known retired generation is accepted.
pub fn close_if_open(id: u64, name: TypeId) -> Result<(), HandleError> {
    match close(id, name) {
        Err(HandleError::Closed) => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unclosed_counts_include_only_live_slots_by_type() {
        for width in [1, 16, 128] {
            let mut table = Table::default();
            let ids: Vec<_> = (0..width)
                .map(|_| table.insert(Box::new(A), TypeId::of::<A>()))
                .collect();
            for &id in &ids {
                drop(table.close(id, TypeId::of::<A>()).unwrap());
            }
            for _ in 0..width {
                table.insert(Box::new(B), TypeId::of::<B>());
            }
            assert_eq!(table.slots.len(), width);
            table.probes.set(0);
            assert_eq!(
                table.unclosed_counts(),
                vec![(std::any::type_name::<B>(), width)]
            );
            assert_eq!(table.probes.get(), width);
            let a = table.insert(Box::new(A), TypeId::of::<A>());
            let lease = table.get(a, TypeId::of::<A>()).unwrap();
            assert!(matches!(
                table.take::<A>(a, TypeId::of::<A>()),
                Err(HandleError::Borrowed)
            ));
            assert_eq!(
                table
                    .unclosed_counts()
                    .iter()
                    .map(|(_, n)| n)
                    .sum::<usize>(),
                width + 1
            );
            drop(table.close(a, TypeId::of::<A>()).unwrap());
            assert_eq!(
                table.unclosed_counts(),
                vec![(std::any::type_name::<B>(), width)]
            );
            drop(lease);
        }
    }
    struct A;
    struct B;
    struct Concurrent;
    struct Nested;
    #[test]
    fn automatic_close_accepts_only_retired_generations() {
        struct Automatic;
        let id = insert(Box::new(Automatic), TypeId::of::<Automatic>());
        close_if_open(id, TypeId::of::<Automatic>()).unwrap();
        close_if_open(id, TypeId::of::<Automatic>()).unwrap();
        assert_eq!(
            close_if_open(0, TypeId::of::<Automatic>()),
            Err(HandleError::Invalid)
        );
        assert_eq!(
            close(id, TypeId::of::<Automatic>()),
            Err(HandleError::Closed)
        );
    }
    #[test]
    fn invalid_closed_wrong_type_and_aba() {
        let mut t = Table::default();
        for id in [0, 1, u64::MAX, 1 << 32] {
            assert!(matches!(
                t.get(id, TypeId::of::<A>()),
                Err(HandleError::Invalid)
            ));
        }
        let a = t.insert(Box::new(42u64), TypeId::of::<A>());
        assert_eq!(
            *t.get(a, TypeId::of::<A>())
                .unwrap()
                .downcast_ref::<u64>()
                .unwrap(),
            42
        );
        assert!(matches!(
            t.get(a, TypeId::of::<B>()),
            Err(HandleError::Invalid)
        ));
        assert!(matches!(
            t.close(a, TypeId::of::<B>()),
            Err(HandleError::Invalid)
        ));
        assert!(matches!(
            t.get(a + (1 << 32), TypeId::of::<A>()),
            Err(HandleError::Invalid)
        ));
        drop(t.close(a, TypeId::of::<A>()).unwrap());
        assert!(matches!(
            t.close(a, TypeId::of::<A>()),
            Err(HandleError::Closed)
        ));
        assert!(matches!(
            t.get(a, TypeId::of::<A>()),
            Err(HandleError::Closed)
        ));
        let b = t.insert(Box::new(7u64), TypeId::of::<B>());
        assert_ne!(a, b);
        assert!(matches!(
            t.get(a, TypeId::of::<A>()),
            Err(HandleError::Closed)
        ));
        assert_eq!(t.slots.len(), 1);
    }
    #[test]
    fn generation_exhaustion_retires_slot() {
        let mut t = Table::default();
        t.insert(Box::new(()), TypeId::of::<A>());
        t.slots[0].generation = u32::MAX;
        let old = (u64::from(u32::MAX) << 32) | 1;
        drop(t.close(old, TypeId::of::<A>()).unwrap());
        let new = t.insert(Box::new(()), TypeId::of::<A>());
        assert_eq!(new as u32, 2);
        assert!(matches!(
            t.get(old, TypeId::of::<A>()),
            Err(HandleError::Closed)
        ));
    }
    #[test]
    fn fragmented_churn_has_one_probe_and_bounded_storage() {
        for n in [16, 64, 256, 1024] {
            let mut t = Table::default();
            let mut ids: Vec<_> = (0..n)
                .map(|i| t.insert(Box::new(i), TypeId::of::<A>()))
                .collect();
            t.probes.set(0);
            for _ in 0..16 {
                for i in (0..n).step_by(2) {
                    drop(t.close(ids[i], TypeId::of::<A>()).unwrap());
                }
                for i in (0..n).step_by(2) {
                    ids[i] = t.insert(Box::new(i), TypeId::of::<A>());
                }
                for &id in &ids {
                    assert!(t.get(id, TypeId::of::<A>()).is_ok());
                }
            }
            assert_eq!(t.probes.get(), 24 * n);
            assert_eq!(t.slots.len(), n);
            assert!(t.free.is_empty());
            eprintln!(
                "handles live={n} operations={} probes={} slots={}",
                24 * n,
                t.probes.get(),
                t.slots.len()
            );
        }
    }
    #[test]
    fn concurrent_leases_close_and_exact_drop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Count(Arc<AtomicUsize>);
        impl Drop for Count {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let id = insert(Box::new(Count(drops.clone())), TypeId::of::<Concurrent>());
        let barrier = Arc::new(std::sync::Barrier::new(9));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let barrier = barrier.clone();
                scope.spawn(move || {
                    let lease = get(id, TypeId::of::<Concurrent>()).unwrap();
                    barrier.wait();
                    barrier.wait();
                    assert!(lease.downcast_ref::<Count>().is_some());
                });
            }
            barrier.wait();
            close(id, TypeId::of::<Concurrent>()).unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert!(matches!(
                get(id, TypeId::of::<Concurrent>()),
                Err(HandleError::Closed)
            ));
            barrier.wait();
        });
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn destructor_can_reenter_table() {
        struct Reenter;
        impl Drop for Reenter {
            fn drop(&mut self) {
                let id = insert(Box::new(()), TypeId::of::<Nested>());
                close(id, TypeId::of::<Nested>()).unwrap();
            }
        }
        let id = insert(Box::new(Reenter), TypeId::of::<Reenter>());
        close(id, TypeId::of::<Reenter>()).unwrap();
    }
    #[test]
    fn concurrent_close_has_exactly_one_winner_and_churn_is_safe() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let id = insert(Box::new(42), TypeId::of::<Concurrent>());
        let wins = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let wins = &wins;
                scope.spawn(move || {
                    match close(id, TypeId::of::<Concurrent>()) {
                        Ok(()) => {
                            wins.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(error) => assert_eq!(error, HandleError::Closed),
                    }
                    for i in 0..1024 {
                        let id = insert(Box::new(i), TypeId::of::<Concurrent>());
                        assert_eq!(
                            *get(id, TypeId::of::<Concurrent>())
                                .unwrap()
                                .downcast_ref::<i32>()
                                .unwrap(),
                            i
                        );
                        close(id, TypeId::of::<Concurrent>()).unwrap();
                    }
                });
            }
        });
        assert_eq!(wins.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn consume_owned_value_invalidates_aliases_and_drops_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Count(Arc<AtomicUsize>);
        impl Drop for Count {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let id = insert(Box::new(Count(drops.clone())), TypeId::of::<Count>());
        let alias = id;
        let value = take::<Count>(id, TypeId::of::<Count>()).unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(matches!(
            get(alias, TypeId::of::<Count>()),
            Err(HandleError::Closed)
        ));
        assert!(matches!(
            take::<Count>(alias, TypeId::of::<Count>()),
            Err(HandleError::Closed)
        ));
        assert_eq!(
            close(alias, TypeId::of::<Count>()),
            Err(HandleError::Closed)
        );
        drop(value);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn consume_borrowed_rejection_preserves_handle() {
        struct Tag;
        let id = insert(Box::new(42u64), TypeId::of::<Tag>());
        let lease = get(id, TypeId::of::<Tag>()).unwrap();
        assert!(matches!(
            take::<u64>(id, TypeId::of::<Tag>()),
            Err(HandleError::Borrowed)
        ));
        assert_eq!(*lease.downcast_ref::<u64>().unwrap(), 42);
        assert!(get(id, TypeId::of::<Tag>()).is_ok());
        drop(lease);
        assert_eq!(*take::<u64>(id, TypeId::of::<Tag>()).unwrap(), 42);
    }

    #[test]
    fn consume_weak_lease_rejection_preserves_handle() {
        struct Tag;
        let id = insert(Box::new(42u64), TypeId::of::<Tag>());
        let lease = get(id, TypeId::of::<Tag>()).unwrap();
        let weak = Arc::downgrade(&lease);
        drop(lease);
        assert!(matches!(
            take::<u64>(id, TypeId::of::<Tag>()),
            Err(HandleError::Borrowed)
        ));
        let upgraded = weak.upgrade().unwrap();
        drop(weak);
        assert_eq!(
            take::<u64>(id, TypeId::of::<Tag>()),
            Err(HandleError::Borrowed)
        );
        let same_generation = get(id, TypeId::of::<Tag>()).unwrap();
        assert!(Arc::ptr_eq(&upgraded, &same_generation));
        drop(same_generation);
        drop(upgraded);
        assert_eq!(*take::<u64>(id, TypeId::of::<Tag>()).unwrap(), 42);
    }

    #[test]
    fn consume_wrong_type_rejection_preserves_handle() {
        struct Tag;
        let id = insert(Box::new(42u64), TypeId::of::<Tag>());
        assert!(matches!(
            take::<String>(id, TypeId::of::<Tag>()),
            Err(HandleError::Invalid)
        ));
        assert!(matches!(
            take::<u64>(id, TypeId::of::<A>()),
            Err(HandleError::Invalid)
        ));
        assert_eq!(*take::<u64>(id, TypeId::of::<Tag>()).unwrap(), 42);
    }

    #[test]
    fn consume_destructor_can_reenter_table() {
        struct Reenter;
        impl Drop for Reenter {
            fn drop(&mut self) {
                let id = insert(Box::new(1u64), TypeId::of::<Nested>());
                assert_eq!(*take::<u64>(id, TypeId::of::<Nested>()).unwrap(), 1);
            }
        }
        let id = insert(Box::new(Reenter), TypeId::of::<Reenter>());
        drop(take::<Reenter>(id, TypeId::of::<Reenter>()).unwrap());
    }

    #[test]
    fn consume_fragmented_churn_has_one_probe() {
        for n in [16, 64, 256, 1024] {
            let mut table = Table::default();
            let mut ids: Vec<_> = (0..n)
                .map(|i| table.insert(Box::new(i), TypeId::of::<A>()))
                .collect();
            table.probes.set(0);
            for _ in 0..16 {
                for i in (0..n).step_by(2) {
                    let lease = table.take::<usize>(ids[i], TypeId::of::<A>()).unwrap();
                    assert_eq!(
                        *Arc::try_unwrap(lease).unwrap().downcast::<usize>().unwrap(),
                        i
                    );
                }
                for i in (0..n).step_by(2) {
                    ids[i] = table.insert(Box::new(i), TypeId::of::<A>());
                }
            }
            assert_eq!(table.probes.get(), 8 * n);
            assert_eq!(table.slots.len(), n);
            assert!(table.free.is_empty());
            eprintln!(
                "consume live={n} operations={} probes={} slots={}",
                8 * n,
                table.probes.get(),
                table.slots.len()
            );
        }
    }
}
