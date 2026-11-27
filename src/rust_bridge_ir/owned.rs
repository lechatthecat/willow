/// An owned lease: close invalidates the public handle but cannot destroy an
/// object still used by a Future or pool job. Adapters borrow through Deref.
pub struct AsyncHandle<T> {
    lease: handles::Lease,
    marker: std::marker::PhantomData<T>,
}
impl<T: std::any::Any + Send + Sync> std::ops::Deref for AsyncHandle<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.lease
            .downcast_ref()
            .expect("opaque adapter type invariant")
    }
}
impl<T: std::any::Any + Send + Sync> AsyncHandle<T> {
    fn new(id: u64, name: std::any::TypeId) -> Self {
        let lease =
            handles::get(id, name).unwrap_or_else(|error| std::panic::panic_any((error, id)));
        assert!(
            lease.downcast_ref::<T>().is_some(),
            "opaque adapter type invariant"
        );
        Self {
            lease,
            marker: std::marker::PhantomData,
        }
    }
}

