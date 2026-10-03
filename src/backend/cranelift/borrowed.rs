//! Borrowed table storage: unit views never copy canonical entries.
use std::ops::{Deref, DerefMut};
#[cfg(test)]
thread_local! { pub(super) static CANONICAL_CLONES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[derive(Debug)]
pub(super) enum Storage<'a, T> {
    Owned(T),
    Shared(&'a T),
    Mutable(&'a mut T),
}
impl<T: Default> Default for Storage<'_, T> {
    fn default() -> Self {
        Self::Owned(T::default())
    }
}
impl<T> Deref for Storage<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match self {
            Self::Owned(value) => value,
            Self::Shared(value) => value,
            Self::Mutable(value) => value,
        }
    }
}
impl<T> DerefMut for Storage<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match self {
            Self::Owned(value) => value,
            Self::Mutable(value) => value,
            Self::Shared(_) => panic!("attempt to mutate frozen codegen metadata"),
        }
    }
}
// Explicit clones are for owning test fixtures. Unit construction uses borrows.
impl<T: Clone> Clone for Storage<'_, T> {
    fn clone(&self) -> Self {
        #[cfg(test)]
        CANONICAL_CLONES.with(|count| count.set(count.get() + 1));
        Self::Owned((**self).clone())
    }
}
