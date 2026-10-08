//! Indexed lexical bindings shared by checking and HIR lowering.
//! Frames own values; per-name stacks identify the innermost live frame.
//! Lookup never scans lexical depth. Same-frame replacement does not push a
//! second index, so exiting a scope restores exactly the preceding binding.
use std::collections::HashMap;

#[cfg(test)]
thread_local! {
    pub(crate) static LOOKUP_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Debug, serde::Serialize)]
#[serde(transparent)]
pub(crate) struct Scopes<T> {
    frames: Vec<HashMap<String, T>>,
    #[serde(skip)]
    by_name: HashMap<String, Vec<usize>>,
}

impl<T> Default for Scopes<T> {
    fn default() -> Self {
        Self {
            frames: Vec::new(),
            by_name: HashMap::new(),
        }
    }
}

impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for Scopes<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let frames = Vec::<HashMap<String, T>>::deserialize(deserializer)?;
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (depth, frame) in frames.iter().enumerate() {
            for name in frame.keys() {
                by_name.entry(name.clone()).or_default().push(depth);
            }
        }
        Ok(Self { frames, by_name })
    }
}

impl<T> Scopes<T> {
    pub(crate) fn with_frame(bindings: impl IntoIterator<Item = (String, T)>) -> Self {
        let mut scopes = Self::default();
        scopes.push();
        for (name, value) in bindings {
            scopes.insert(name, value);
        }
        scopes
    }

    pub(crate) fn len(&self) -> usize {
        self.frames.len()
    }

    pub(crate) fn push(&mut self) {
        self.frames.push(HashMap::new());
    }

    pub(crate) fn pop(&mut self) {
        if let Some(frame) = self.frames.pop() {
            for name in frame.keys() {
                let indices = self.by_name.get_mut(name).expect("indexed binding");
                let removed = indices.pop();
                debug_assert_eq!(removed, Some(self.frames.len()));
                if indices.is_empty() {
                    self.by_name.remove(name);
                }
            }
        }
    }

    pub(crate) fn insert(&mut self, name: String, value: T) {
        let depth = self.frames.len();
        if let Some(frame) = self.frames.last_mut() {
            if !frame.contains_key(&name) {
                self.by_name
                    .entry(name.clone())
                    .or_default()
                    .push(depth - 1);
            }
            frame.insert(name, value);
        }
    }

    fn owner(&self, name: &str) -> Option<usize> {
        #[cfg(test)]
        LOOKUP_PROBES.with(|n| n.set(n.get() + 1));
        self.by_name.get(name)?.last().copied()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&T> {
        let owner = self.owner(name)?;
        #[cfg(test)]
        LOOKUP_PROBES.with(|n| n.set(n.get() + 1));
        self.frames[owner].get(name)
    }

    pub(crate) fn get_mut(&mut self, name: &str) -> Option<&mut T> {
        let owner = self.owner(name)?;
        #[cfg(test)]
        LOOKUP_PROBES.with(|n| n.set(n.get() + 1));
        self.frames[owner].get_mut(name)
    }

    pub(crate) fn current(&self, name: &str) -> Option<&T> {
        self.frames.last()?.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_scopes_restore_shadowed_and_replaced_bindings() {
        let mut scopes = Scopes::default();
        scopes.insert("ignored".into(), 0);
        assert_eq!(scopes.get("ignored"), None);
        scopes.pop();
        scopes.push();
        scopes.insert("x".into(), 1);
        scopes.insert("outer".into(), 2);
        scopes.push();
        assert_eq!(scopes.current("x"), None);
        assert_eq!(scopes.get("x"), Some(&1));
        *scopes.get_mut("outer").unwrap() = 3;
        scopes.insert("x".into(), 4);
        scopes.insert("x".into(), 5);
        assert_eq!(scopes.current("x"), Some(&5));
        scopes.push();
        scopes.insert("x".into(), 6);
        assert_eq!(scopes.get("x"), Some(&6));
        scopes.pop();
        assert_eq!(scopes.get("x"), Some(&5));
        scopes.pop();
        assert_eq!(scopes.get("x"), Some(&1));
        assert_eq!(scopes.get("outer"), Some(&3));
        scopes.pop();
        assert!(scopes.by_name.is_empty());
        assert_eq!(scopes.get("x"), None);
        assert_eq!(scopes.get_mut("x"), None);
    }

    #[test]
    fn indexed_scopes_deserialize_existing_wire_and_rebuild_indices() {
        let wire = serde_json::json!([{"x": 1, "outer": 2}, {}, {"x": 3}]);
        let mut scopes: Scopes<i64> = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&scopes).unwrap(), wire);
        assert_eq!(scopes.get("x"), Some(&3));
        assert_eq!(scopes.get("outer"), Some(&2));
        scopes.insert("x".into(), 4);
        scopes.pop();
        assert_eq!(scopes.get("x"), Some(&1));
        scopes.pop();
        *scopes.get_mut("x").unwrap() = 5;
        assert_eq!(scopes.current("x"), Some(&5));
    }

    #[test]
    fn indexed_scopes_lookup_probes_do_not_depend_on_depth() {
        for depth in [8, 16, 32, 64, 128] {
            let mut scopes = Scopes::with_frame([("seed".into(), 1)]);
            for _ in 0..depth {
                scopes.push();
                scopes.insert("shadow".into(), 2);
            }
            LOOKUP_PROBES.with(|n| n.set(0));
            for _ in 0..depth {
                assert_eq!(scopes.get("seed"), Some(&1));
                assert_eq!(scopes.get("shadow"), Some(&2));
                assert_eq!(scopes.get("absent"), None);
                assert_eq!(scopes.get_mut("seed"), Some(&mut 1));
            }
            assert_eq!(LOOKUP_PROBES.with(|n| n.get()), 7 * depth);
            for _ in 0..depth {
                scopes.pop();
            }
            assert_eq!(scopes.get("seed"), Some(&1));
            assert_eq!(scopes.get("shadow"), None);
        }
    }
}
