// Shared source compiled in the generated bridge so Rust owns every allocation.
// No native pointer is exported to Willow. Explicit close only; no GC finalizer.
use std::any::{Any, TypeId};
use std::sync::{Arc, Mutex, OnceLock};
pub type Lease = Arc<Box<dyn Any + Send + Sync>>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleError {
    Invalid,
    Closed,
}

struct Slot {
    generation: u32,
    name: TypeId,
    value: Option<Lease>,
}
#[derive(Default)]
pub struct Table {
    slots: Vec<Slot>,
    free: Vec<u32>,
    #[cfg(test)]
    probes: std::cell::Cell<usize>,
}
impl Table {
    pub fn insert<T: Any + Send + Sync>(&mut self, value: Box<T>, name: TypeId) -> u64 {
        let value: Box<dyn Any + Send + Sync> = value;
        let value = Some(Arc::new(value));
        let index = if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.generation += 1;
            slot.name = name;
            slot.value = value;
            index
        } else {
            let index = u32::try_from(self.slots.len()).expect("Rust handle table exhausted");
            // Low word zero is reserved for invalid IDs.
            assert!(index < u32::MAX, "Rust handle table exhausted");
            self.slots.push(Slot {
                generation: 1,
                name,
                value,
            });
            index
        };
        (u64::from(self.slots[index as usize].generation) << 32) | (u64::from(index) + 1)
    }
    fn index(&self, id: u64, name: TypeId) -> Result<usize, HandleError> {
        let index = (id as u32).checked_sub(1).ok_or(HandleError::Invalid)? as usize;
        #[cfg(test)]
        self.probes.set(self.probes.get() + 1);
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
        let slot = &mut self.slots[index];
        let value = slot.value.take().unwrap();
        if slot.generation < u32::MAX {
            self.free.push(index as u32);
        }
        Ok(value)
    }
}
fn table() -> &'static Mutex<Table> {
    static TABLE: OnceLock<Mutex<Table>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(Table::default()))
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
pub fn close(id: u64, name: TypeId) -> Result<(), HandleError> {
    let removed = table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .close(id, name)?;
    drop(removed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct A;
    struct B;
    struct Concurrent;
    struct Nested;
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
}
