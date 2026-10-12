//! Path-compressed word radix index of allocation starts. Extent owners still
//! validate membership, so predecessor hits in gaps never become allocations.
const NONE: usize = usize::MAX;
#[derive(Clone, Copy)]
struct Node {
    bit: Option<u32>,
    children: [usize; 2],
    entry: (usize, usize),
    min: usize,
    max: usize,
}
#[derive(Default)]
pub(super) struct AddressIndex {
    nodes: Vec<Node>,
    free: Vec<usize>,
    root: Option<usize>,
    len: usize,
}
impl AddressIndex {
    fn allocate(&mut self, node: Node) -> usize {
        if let Some(index) = self.free.pop() {
            self.nodes[index] = node;
            index
        } else {
            let index = self.nodes.len();
            self.nodes.push(node);
            index
        }
    }
    fn leaf_node(&mut self, start: usize, value: usize) -> usize {
        let index = self.allocate(Node {
            bit: None,
            children: [NONE; 2],
            entry: (start, value),
            min: NONE,
            max: NONE,
        });
        self.nodes[index].min = index;
        self.nodes[index].max = index;
        index
    }
    fn refresh(&mut self, index: usize) {
        let [left, right] = self.nodes[index].children;
        self.nodes[index].min = self.nodes[left].min;
        self.nodes[index].max = self.nodes[right].max;
    }
    fn visit(&self, index: usize) -> Node {
        #[cfg(test)]
        COMPARISONS.set(COMPARISONS.get() + 1);
        self.nodes[index]
    }
    fn search_leaf(&self, start: usize) -> Option<usize> {
        let mut index = self.root?;
        loop {
            let node = self.visit(index);
            let Some(bit) = node.bit else {
                return Some(index);
            };
            index = node.children[(start >> bit) & 1];
        }
    }
    pub(super) fn insert(&mut self, start: usize, value: usize) {
        let Some(root) = self.root else {
            self.root = Some(self.leaf_node(start, value));
            self.len = 1;
            return;
        };
        let existing = self.search_leaf(start).unwrap();
        let difference = self.nodes[existing].entry.0 ^ start;
        assert_ne!(difference, 0, "duplicate heap region start");
        let split_bit = usize::BITS - 1 - difference.leading_zeros();
        let mut path = [NONE; usize::BITS as usize];
        let mut depth = 0;
        let mut index = root;
        while self.nodes[index].bit.is_some_and(|bit| bit > split_bit) {
            path[depth] = index;
            depth += 1;
            index = self.nodes[index].children[(start >> self.nodes[index].bit.unwrap()) & 1];
        }
        let leaf = self.leaf_node(start, value);
        let branch = (start >> split_bit) & 1;
        let mut children = [index; 2];
        children[branch] = leaf;
        let split = self.allocate(Node {
            bit: Some(split_bit),
            children,
            entry: (0, 0),
            min: NONE,
            max: NONE,
        });
        self.refresh(split);
        if depth == 0 {
            self.root = Some(split);
        } else {
            let parent = path[depth - 1];
            let side = (start >> self.nodes[parent].bit.unwrap()) & 1;
            self.nodes[parent].children[side] = split;
        }
        for &parent in path[..depth].iter().rev() {
            self.refresh(parent);
        }
        self.len += 1;
    }
    pub(super) fn candidate(&self, address: usize) -> Option<usize> {
        let mut index = self.root?;
        loop {
            let node = self.visit(index);
            let min = self.nodes[node.min].entry;
            let max = self.nodes[node.max].entry;
            if address < min.0 {
                return None;
            }
            if address >= max.0 {
                return Some(max.1);
            }
            let [left, right] = node.children;
            debug_assert!(node.bit.is_some());
            index = if address >= self.nodes[self.nodes[right].min].entry.0 {
                right
            } else {
                left
            };
        }
    }
    pub(super) fn exact(&self, start: usize) -> Option<usize> {
        let entry = self.nodes[self.search_leaf(start)?].entry;
        (entry.0 == start).then_some(entry.1)
    }
    pub(super) fn move_index(&mut self, start: usize, value: usize) {
        let leaf = self.search_leaf(start).expect("live extent");
        assert_eq!(self.nodes[leaf].entry.0, start);
        self.nodes[leaf].entry.1 = value;
    }
    pub(super) fn remove(&mut self, start: usize) {
        let mut path = [NONE; usize::BITS as usize];
        let mut depth = 0;
        let mut index = self.root.expect("live extent");
        while let Some(bit) = self.nodes[index].bit {
            path[depth] = index;
            depth += 1;
            index = self.nodes[index].children[(start >> bit) & 1];
        }
        assert_eq!(self.nodes[index].entry.0, start, "live extent");
        self.free.push(index);
        self.len -= 1;
        if depth == 0 {
            self.root = None;
            return;
        }
        let parent = path[depth - 1];
        let side = (start >> self.nodes[parent].bit.unwrap()) & 1;
        let sibling = self.nodes[parent].children[1 - side];
        self.free.push(parent);
        if depth == 1 {
            self.root = Some(sibling);
        } else {
            let grandparent = path[depth - 2];
            let side = (start >> self.nodes[grandparent].bit.unwrap()) & 1;
            self.nodes[grandparent].children[side] = sibling;
            for &ancestor in path[..depth - 1].iter().rev() {
                self.refresh(ancestor);
            }
        }
    }
    pub(super) fn matches(&self, count: usize, start_at: impl Fn(usize) -> Option<usize>) -> bool {
        fn walk(
            tree: &AddressIndex,
            index: usize,
            start_at: &impl Fn(usize) -> Option<usize>,
        ) -> bool {
            let node = tree.nodes[index];
            if node.bit.is_none() {
                return start_at(node.entry.1) == Some(node.entry.0);
            }
            node.children
                .into_iter()
                .all(|child| walk(tree, child, start_at))
        }
        self.len == count && self.root.is_none_or(|root| walk(self, root, &start_at))
    }
    /// One active-node traversal, including pruning; no historical slab scan or
    /// repeated insertion. A compressed binary tree has exactly 2*leaves-1 nodes.
    pub(super) fn remap(&mut self, positions: &[Option<usize>]) {
        fn walk(tree: &mut AddressIndex, index: usize, positions: &[Option<usize>]) -> usize {
            let node = tree.nodes[index];
            if node.bit.is_none() {
                if let Some(value) = positions[node.entry.1] {
                    tree.nodes[index].entry.1 = value;
                    return index;
                }
                tree.free.push(index);
                tree.len -= 1;
                return NONE;
            }
            let left = walk(tree, node.children[0], positions);
            let right = walk(tree, node.children[1], positions);
            if left == NONE || right == NONE {
                tree.free.push(index);
                return if left == NONE { right } else { left };
            }
            tree.nodes[index].children = [left, right];
            tree.refresh(index);
            index
        }
        if let Some(root) = self.root {
            let next = walk(self, root, positions);
            self.root = (next != NONE).then_some(next);
        }
    }
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
thread_local! { static COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(super) fn take_comparisons() -> usize {
    COMPARISONS.replace(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn radix_lookup_and_remap_have_word_bounded_work() {
        for count in [1_000usize, 10_000, 100_000] {
            let mut index = AddressIndex::default();
            for i in 0..count {
                index.insert(1024 + i * 128, i);
            }
            assert_eq!(index.candidate(1023), None);
            COMPARISONS.set(0);
            for i in 0..count {
                assert_eq!(index.candidate(1024 + i * 128 + 127), Some(i));
            }
            let visits = COMPARISONS.get();
            assert!(visits <= count * (2 * usize::BITS as usize + 1));
            println!(
                "radix regions={count} queries={count} visits={visits} nodes={} metadata_bytes={}",
                index.nodes.len(),
                index.nodes.len() * size_of::<Node>()
            );
            let positions: Vec<_> = (0..count).map(|i| (i % 2 == 1).then_some(i / 2)).collect();
            index.remap(&positions);
            for (i, &position) in positions.iter().enumerate() {
                assert_eq!(index.exact(1024 + i * 128), position);
            }
            assert!(index.matches(count / 2, |i| Some(1024 + (2 * i + 1) * 128)));
            for i in (1..count).step_by(2) {
                index.remove(1024 + i * 128);
            }
            assert_eq!(index.candidate(usize::MAX), None);
            assert_eq!(index.free.len(), index.nodes.len());
        }
    }
    #[test]
    fn reclaimed_radix_nodes_can_be_reinserted_and_remapped() {
        let mut index = AddressIndex::default();
        let keys: Vec<_> = (0..256usize)
            .map(|i| i.wrapping_mul(usize::MAX / 257))
            .collect();
        for (value, &key) in keys.iter().enumerate() {
            index.insert(key, value);
        }
        let capacity = index.nodes.len();
        for &key in &keys {
            index.remove(key);
        }
        for round in 0..4 {
            let mut oracle = std::collections::BTreeMap::new();
            for (value, &key) in keys.iter().rev().enumerate() {
                let key = key ^ usize::MAX;
                index.insert(key, value);
                oracle.insert(key, value);
            }
            assert_eq!(index.nodes.len(), capacity);
            assert_eq!(index.nodes.len() - index.free.len(), 2 * index.len - 1);
            for (&key, &value) in &oracle {
                assert_eq!(index.exact(key), Some(value));
                for query in [key.saturating_sub(1), key, key.saturating_add(1)] {
                    assert_eq!(
                        index.candidate(query),
                        oracle.range(..=query).next_back().map(|(_, &v)| v)
                    );
                }
            }
            index.remap(&vec![None; keys.len()]);
            assert_eq!(index.len, 0);
            assert_eq!(index.free.len(), capacity);
            assert_eq!(index.candidate(usize::MAX), None);
            eprintln!("radix reuse round={round} nodes={capacity} live=0");
        }
    }

    #[test]
    fn sparse_boundaries_and_churn_match_ordered_oracle() {
        let mut index = AddressIndex::default();
        let mut oracle = std::collections::BTreeMap::new();
        let mut values = vec![
            0,
            1,
            usize::MAX,
            usize::MAX - 1,
            1usize << (usize::BITS - 1),
        ];
        let mut seed = 17usize;
        for _ in 0..2000 {
            seed = seed
                .wrapping_mul(6364136223846793005u64 as usize)
                .wrapping_add(1);
            values.push(seed);
        }
        for (i, &key) in values.iter().enumerate() {
            index.insert(key, i);
            oracle.insert(key, i);
        }
        for &key in &values {
            for query in [key, key.saturating_sub(1), key.saturating_add(1)] {
                assert_eq!(
                    index.candidate(query),
                    oracle.range(..=query).next_back().map(|(_, &v)| v)
                );
            }
        }
        for (i, &key) in values.iter().enumerate() {
            if i % 2 == 0 {
                index.remove(key);
                oracle.remove(&key);
            } else {
                index.move_index(key, i + 1);
                oracle.insert(key, i + 1);
            }
        }
        for &query in &values {
            assert_eq!(
                index.candidate(query),
                oracle.range(..=query).next_back().map(|(_, &v)| v)
            );
        }
        index.clear();
        assert_eq!(index.candidate(0), None);
    }
}
