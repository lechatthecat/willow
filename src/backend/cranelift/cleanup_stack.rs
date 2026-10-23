//! Persistent machine-resource bindings for graph-owned cleanup states.
//! Cloning/restoring a head is O(1); dropping a deep unshared chain is iterative.
use std::rc::Rc;

pub(super) struct Chain<T> {
    head: Option<Rc<Node<T>>>,
}
struct Node<T> {
    value: T,
    parent: Chain<T>,
    len: usize,
}
impl<T> Default for Chain<T> {
    fn default() -> Self {
        Self { head: None }
    }
}
impl<T> Clone for Chain<T> {
    fn clone(&self) -> Self {
        Self {
            head: self.head.clone(),
        }
    }
}
impl<T> Drop for Chain<T> {
    fn drop(&mut self) {
        let mut head = self.head.take();
        while let Some(node) = head {
            match Rc::try_unwrap(node) {
                Ok(mut node) => head = node.parent.head.take(),
                Err(_) => break,
            }
        }
    }
}
impl<T> Chain<T> {
    pub fn len(&self) -> usize {
        self.head.as_ref().map_or(0, |node| node.len)
    }
    pub fn is_empty(&self) -> bool {
        self.head.is_none()
    }
    pub fn last(&self) -> Option<&T> {
        self.head.as_ref().map(|node| &node.value)
    }
    pub fn push(&mut self, value: T) {
        self.head = Some(Rc::new(Node {
            value,
            parent: self.clone(),
            len: self.len() + 1,
        }));
    }
    pub fn pop(&mut self) {
        self.head = self.head.as_ref().and_then(|node| node.parent.head.clone());
    }
    pub fn iter_rev(&self) -> impl Iterator<Item = &T> {
        std::iter::successors(self.head.as_deref(), |node| node.parent.head.as_deref())
            .map(|node| &node.value)
    }
}

#[cfg(test)]
mod tests {
    use super::Chain;
    use std::cell::Cell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Counts {
        created: Cell<usize>,
        cloned: Cell<usize>,
        dropped: Cell<usize>,
    }

    struct Payload {
        index: usize,
        counts: Rc<Counts>,
    }

    impl Payload {
        fn new(index: usize, counts: &Rc<Counts>) -> Self {
            counts.created.set(counts.created.get() + 1);
            Self {
                index,
                counts: Rc::clone(counts),
            }
        }
    }

    impl Clone for Payload {
        fn clone(&self) -> Self {
            self.counts.cloned.set(self.counts.cloned.get() + 1);
            Self::new(self.index, &self.counts)
        }
    }

    impl Drop for Payload {
        fn drop(&mut self) {
            self.counts.dropped.set(self.counts.dropped.get() + 1);
        }
    }

    #[test]
    fn cleanup_chain_snapshot_fanout_has_zero_payload_clones() {
        for depth in [8, 64, 512] {
            for fanout in [4, 32, 256] {
                let counts = Rc::new(Counts::default());
                let mut chain = Chain::default();
                for index in 0..depth {
                    chain.push(Payload::new(index, &counts));
                }
                let successors: Vec<_> = (0..fanout).map(|_| chain.clone()).collect();
                let mut installed = Chain::default();
                for successor in &successors {
                    installed = successor.clone();
                    assert_eq!(installed.len(), depth);
                    assert_eq!(installed.last().unwrap().index, depth - 1);
                }
                assert_eq!(counts.created.get(), depth);
                assert_eq!(counts.cloned.get(), 0);
                assert_eq!(counts.dropped.get(), 0);
                drop(chain);
                drop(successors);
                assert_eq!(counts.dropped.get(), 0);
                drop(installed);
                assert_eq!(counts.dropped.get(), depth);
                println!(
                    "cleanup_chain depth={depth} fanout={fanout} created={} payload_clones={} dropped={}",
                    counts.created.get(),
                    counts.cloned.get(),
                    counts.dropped.get(),
                );
            }
        }
    }

    #[test]
    fn cleanup_chain_deep_drop_uses_bounded_stack() {
        std::thread::Builder::new()
            .name("cleanup-chain-small-stack".into())
            .stack_size(128 * 1024)
            .spawn(|| {
                let counts = Rc::new(Counts::default());
                let mut chain = Chain::default();
                let depth = 20_000;
                for index in 0..depth {
                    chain.push(Payload::new(index, &counts));
                }
                assert_eq!(chain.len(), depth);
                drop(chain);
                assert_eq!(counts.created.get(), depth);
                assert_eq!(counts.cloned.get(), 0);
                assert_eq!(counts.dropped.get(), depth);
                println!("cleanup_chain depth={depth} stack_bytes=131072 dropped={depth}");
            })
            .expect("spawn bounded-stack cleanup drop test")
            .join()
            .expect("bounded-stack cleanup drop test panicked");
    }
}
