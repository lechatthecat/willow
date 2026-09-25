//! Ordered dispatch slots with an interned-name index. Slot order is ABI data.
use super::ids::FunctionId;
use std::collections::HashMap;

/// A method spelling, independent of the class/interface that owns its slots.
/// Reuses the compiler interner rather than maintaining a second symbol table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), derive(Hash))]
struct MethodId(FunctionId);
impl MethodId {
    fn new(name: &str) -> Self {
        Self(FunctionId::free(name))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MethodSlots {
    names: Vec<String>,
    index: HashMap<MethodId, usize>,
}
impl MethodSlots {
    pub fn slot_of(&self, name: &str) -> Option<usize> {
        self.index.get(&MethodId::new(name)).copied()
    }

    /// Inherited overrides retain the first declaration's slot.
    pub fn insert(&mut self, name: &str) {
        let next = self.names.len();
        self.index.entry(MethodId::new(name)).or_insert_with(|| {
            self.names.push(name.to_owned());
            next
        });
    }

    pub fn as_slice(&self) -> &[String] {
        &self.names
    }
    pub fn iter(&self) -> std::slice::Iter<'_, String> {
        self.names.iter()
    }
    pub fn len(&self) -> usize {
        self.names.len()
    }
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}
impl From<Vec<String>> for MethodSlots {
    fn from(names: Vec<String>) -> Self {
        let mut index = HashMap::with_capacity(names.len());
        for (slot, name) in names.iter().enumerate() {
            index.entry(MethodId::new(name)).or_insert(slot);
        }
        Self { names, index }
    }
}
impl<'a> IntoIterator for &'a MethodSlots {
    type Item = &'a String;
    type IntoIter = std::slice::Iter<'a, String>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl serde::Serialize for MethodSlots {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&self.names, serializer)
    }
}
impl<'de> serde::Deserialize<'de> for MethodSlots {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<String>::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::hash::{Hash, Hasher};

    thread_local! {
        static KEY_HASHES: Cell<usize> = const { Cell::new(0) };
    }
    impl Hash for MethodId {
        fn hash<H: Hasher>(&self, state: &mut H) {
            KEY_HASHES.with(|count| count.set(count.get() + 1));
            self.0.hash(state);
        }
    }

    #[test]
    fn order_overrides_and_missing_names() {
        let mut slots = MethodSlots::default();
        for name in ["z", "a", "z", "b", "a"] {
            slots.insert(name);
        }
        assert_eq!(slots.as_slice(), ["z", "a", "b"]);
        assert_eq!(slots.slot_of("z"), Some(0));
        assert_eq!(slots.slot_of("a"), Some(1));
        assert_eq!(slots.slot_of("b"), Some(2));
        assert_eq!(slots.slot_of("missing"), None);
        let imported = MethodSlots::from(vec!["z".into(), "a".into(), "z".into()]);
        assert_eq!(imported.as_slice(), ["z", "a", "z"]);
        assert_eq!(imported.slot_of("z"), Some(0));
        assert_eq!(imported.clone().slot_of("a"), Some(1));
        assert_eq!(MethodSlots::default().slot_of("z"), None);
    }

    #[test]
    fn increasing_width_repeated_overrides_and_calls() {
        for width in [1, 16, 256, 4096] {
            let names: Vec<_> = (0..width).map(|i| format!("method_{i:04}")).collect();
            let mut slots = MethodSlots::default();
            for _ in 0..4 {
                for name in &names {
                    slots.insert(name);
                }
            }
            assert_eq!(slots.len(), width);
            assert_eq!(slots.index.len(), width);
            KEY_HASHES.with(|count| count.set(0));
            for _ in 0..8 {
                for (slot, name) in names.iter().enumerate().rev() {
                    assert_eq!(slots.slot_of(name), Some(slot));
                }
                assert_eq!(slots.slot_of("absent"), None);
            }
            let hashes = KEY_HASHES.with(Cell::get);
            assert_eq!(hashes, 8 * (width + 1));
            eprintln!(
                "width={width} inserts={} lookups={} key_hashes={hashes} stored_names={} indexed_names={}",
                4 * width,
                8 * (width + 1),
                slots.len(),
                slots.index.len()
            );
        }
    }
}
