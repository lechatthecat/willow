//! Observed generic-state maps. Journals own keys and fingerprints, never maps.
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};
use serde_json::Value;
use std::{
    borrow::Borrow,
    cell::RefCell,
    collections::{HashMap, HashSet, hash_map},
    hash::{Hash, Hasher},
    ops::{Deref, DerefMut, Index},
};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Key {
    pub field: String,
    pub key: Option<Value>,
    #[serde(skip)]
    encoded: Option<String>,
}
impl Key {
    pub fn new(field: impl Into<String>, key: Option<Value>) -> Self {
        let encoded = key
            .as_ref()
            .map(|key| serde_json::to_string(key).expect("journal key"));
        Self {
            field: field.into(),
            key,
            encoded,
        }
    }
}
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.field == other.field && self.encoded == other.encoded
    }
}
impl Eq for Key {}
impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.field.hash(state);
        self.encoded.hash(state);
    }
}
impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            field: String,
            key: Option<Value>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self::new(wire.field, wire.key))
    }
}

/// Symbol changes since the preceding dispatch, followed by its observed result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DispatchEvent {
    pub writes: Vec<(Key, Value)>,
    pub read: Value,
    pub result: bool,
}

#[derive(Default)]
pub(crate) struct Journal {
    pub reads: HashMap<Key, Value>,
    pub touched: HashSet<Key>,
    pub dispatch: Vec<DispatchEvent>,
    dirty_symbols: HashSet<Key>,
    // Only overwritten entries are retained. A whole-map read can then recover
    // its entry state without an O(map size) snapshot around every mutation.
    before: HashMap<Key, Value>,
}
thread_local! {
    static JOURNALS: RefCell<Vec<(u64, Journal)>> = const { RefCell::new(Vec::new()) };
    static NEXT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
pub(crate) fn is_active() -> bool {
    JOURNALS.with(|stack| !stack.borrow().is_empty())
}
pub(crate) struct Capture {
    id: u64,
    finished: bool,
}
pub(crate) fn begin() -> Capture {
    let id = NEXT.with(|next| {
        let id = next.get();
        next.set(id.wrapping_add(1));
        id
    });
    JOURNALS.with(|stack| stack.borrow_mut().push((id, Journal::default())));
    Capture {
        id,
        finished: false,
    }
}
impl Capture {
    pub fn finish(mut self) -> Journal {
        self.finished = true;
        JOURNALS.with(|stack| {
            let mut stack = stack.borrow_mut();
            let index = stack
                .iter()
                .position(|(id, _)| *id == self.id)
                .expect("live journal");
            stack.remove(index).1
        })
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if !self.finished {
            JOURNALS.with(|stack| stack.borrow_mut().retain(|(id, _)| *id != self.id));
        }
    }
}
fn canonical(value: impl Serialize) -> Value {
    crate::compiler_db::syntax::semantic(&serde_json::to_value(value).expect("journal value"))
}
fn entry<V: Serialize>(value: Option<&V>) -> Value {
    canonical((value.is_some(), value))
}
fn own_write(journal: &Journal, key: &Key) -> bool {
    journal.touched.contains(key) || journal.touched.contains(&Key::new(&key.field, None))
}
pub(crate) fn read(key: Key, value: Value) {
    JOURNALS.with(|stack| {
        for (_, journal) in stack.borrow_mut().iter_mut() {
            if !own_write(journal, &key) {
                journal
                    .reads
                    .entry(key.clone())
                    .or_insert_with(|| value.clone());
            }
        }
    });
}
fn baseline(journal: &Journal, field: &str, current: &Value) -> Value {
    let whole = Key::new(field, None);
    if let Some(before) = journal.before.get(&whole) {
        return before.clone();
    }
    let mut entries: HashMap<String, (Value, Value)> = current
        .as_array()
        .expect("map entries")
        .iter()
        .map(|pair| {
            let pair = pair.as_array().expect("map pair");
            (
                serde_json::to_string(&pair[0]).expect("map key"),
                (pair[0].clone(), pair[1].clone()),
            )
        })
        .collect();
    for (key, value) in &journal.before {
        if key.field != field {
            continue;
        }
        let Some(encoded) = &key.encoded else {
            continue;
        };
        let parts = value.as_array().expect("entry fingerprint");
        if parts[0] == Value::Bool(true) {
            entries.insert(
                encoded.clone(),
                (key.key.clone().unwrap(), parts[1].clone()),
            );
        } else {
            entries.remove(encoded);
        }
    }
    let mut entries: Vec<_> = entries.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Array(
        entries
            .into_iter()
            .map(|(_, (key, value))| Value::Array(vec![key, value]))
            .collect(),
    )
}
fn read_whole(field: &str, value: Value) {
    JOURNALS.with(|stack| {
        for (_, journal) in stack.borrow_mut().iter_mut() {
            let key = Key::new(field, None);
            if !own_write(journal, &key) && !journal.reads.contains_key(&key) {
                let before = baseline(journal, field, &value);
                journal.reads.insert(key, before);
            }
        }
    });
}
pub(crate) fn write(key: Key, before: Value) {
    JOURNALS.with(|stack| {
        for (_, journal) in stack.borrow_mut().iter_mut() {
            if !own_write(journal, &key) {
                let before = if key.key.is_none() {
                    baseline(journal, &key.field, &before)
                } else {
                    before.clone()
                };
                journal.before.entry(key.clone()).or_insert(before);
            }
            journal.touched.insert(key.clone());
            if key.field == "symbols" {
                journal.dirty_symbols.insert(key.clone());
            }
        }
    });
}

/// Drain only changes since this capture's previous dispatch checkpoint.
pub(crate) fn take_dirty_symbols() -> Vec<Key> {
    JOURNALS.with(|stack| {
        let mut stack = stack.borrow_mut();
        let Some((_, journal)) = stack.last_mut() else {
            return Vec::new();
        };
        let mut keys: Vec<_> = journal.dirty_symbols.drain().collect();
        keys.sort_by(|a, b| {
            a.field
                .cmp(&b.field)
                .then_with(|| a.encoded.cmp(&b.encoded))
        });
        keys
    })
}
pub(crate) fn record_dispatch(event: DispatchEvent) {
    JOURNALS.with(|stack| {
        if let Some((_, journal)) = stack.borrow_mut().last_mut() {
            journal.dispatch.push(event);
        }
    });
}

#[derive(Clone, Debug)]
pub(crate) struct Map<K: Eq + Hash + Serialize, V: Serialize> {
    label: &'static str,
    inner: HashMap<K, V>,
    // Cloning a map clones this memo independently: divergent clones must not
    // publish fingerprints of different values into one shared cache.
    fingerprints: RefCell<HashMap<String, Value>>,
}
impl<K: Eq + Hash + Serialize, V: Serialize> Default for Map<K, V> {
    fn default() -> Self {
        Self::new("")
    }
}
impl<K: Eq + Hash + Serialize, V: Serialize> Map<K, V> {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            inner: HashMap::new(),
            fingerprints: RefCell::new(HashMap::new()),
        }
    }
    pub fn raw(&self) -> &HashMap<K, V> {
        &self.inner
    }
    fn key<Q: Serialize + ?Sized>(&self, key: &Q) -> Key {
        Key::new(
            self.label,
            Some(serde_json::to_value(key).expect("map key")),
        )
    }
    /// One serialization per key/value epoch, shared by every reader of this
    /// map. Persisted dependencies retain only the fixed-size digest, while
    /// output_value continues to carry the complete replay payload.
    fn fingerprint(&self, key: &Key, value: Option<&V>) -> Value {
        let encoded = key.encoded.as_ref().expect("entry key");
        let cached = self.fingerprints.borrow().get(encoded).cloned();
        if let Some(cached) = cached {
            return cached;
        }
        let fingerprint = if let Some(value) = value {
            use sha2::{Digest, Sha256};
            let normalized = canonical(value);
            let digest = Sha256::digest(serde_json::to_vec(&normalized).expect("map fingerprint"));
            serde_json::json!([true, format!("{:x}", crate::hex::Hex(&digest))])
        } else {
            serde_json::json!([false, null])
        };
        self.fingerprints
            .borrow_mut()
            .insert(encoded.clone(), fingerprint.clone());
        fingerprint
    }
    fn whole(&self) -> Value {
        let mut pairs: Vec<_> = self
            .inner
            .iter()
            .map(|(key, value)| {
                let key = self.key(key);
                let fingerprint = self.fingerprint(&key, Some(value));
                (
                    key.encoded.unwrap(),
                    Value::Array(vec![key.key.unwrap(), fingerprint[1].clone()]),
                )
            })
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        Value::Array(pairs.into_iter().map(|(_, pair)| pair).collect())
    }
    fn observe_whole(&self) {
        if is_active() {
            read_whole(self.label, self.whole());
        }
    }
    pub fn get<Q: Hash + Eq + Serialize + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        let value = self.inner.get(key);
        if is_active() {
            let key = self.key(key);
            let fingerprint = self.fingerprint(&key, value);
            read(key, fingerprint);
        }
        value
    }
    pub fn contains_key<Q: Hash + Eq + Serialize + ?Sized>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.get(key).is_some()
    }
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let active = is_active();
        if active || !self.fingerprints.get_mut().is_empty() {
            let observed = self.key(&key);
            if active {
                write(
                    observed.clone(),
                    self.fingerprint(&observed, self.inner.get(&key)),
                );
            }
            self.fingerprints
                .get_mut()
                .remove(observed.encoded.as_ref().unwrap());
        }
        self.inner.insert(key, value)
    }
    #[cfg(test)]
    pub fn remove<Q: Hash + Eq + Serialize + ?Sized>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        let active = is_active();
        if active || !self.fingerprints.get_mut().is_empty() {
            let observed = self.key(key);
            if active {
                write(
                    observed.clone(),
                    self.fingerprint(&observed, self.inner.get(key)),
                );
            }
            self.fingerprints
                .get_mut()
                .remove(observed.encoded.as_ref().unwrap());
        }
        self.inner.remove(key)
    }
    pub fn entry(&mut self, key: K) -> hash_map::Entry<'_, K, V> {
        let active = is_active();
        if active || !self.fingerprints.get_mut().is_empty() {
            let observed = self.key(&key);
            if active {
                let fingerprint = self.fingerprint(&observed, self.inner.get(&key));
                read(observed.clone(), fingerprint.clone());
                write(observed.clone(), fingerprint);
            }
            // Entry exposes &mut V, including occupied entries returned by
            // or_insert. Invalidate before lending that mutable reference.
            self.fingerprints
                .get_mut()
                .remove(observed.encoded.as_ref().unwrap());
        }
        self.inner.entry(key)
    }
    pub fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (key, value) in iter {
            self.insert(key, value);
        }
    }
    pub fn iter(&self) -> hash_map::Iter<'_, K, V> {
        self.observe_whole();
        self.inner.iter()
    }
    pub fn values(&self) -> hash_map::Values<'_, K, V> {
        self.observe_whole();
        self.inner.values()
    }
    pub fn len(&self) -> usize {
        self.observe_whole();
        self.inner.len()
    }
    pub fn is_empty(&self) -> bool {
        self.observe_whole();
        self.inner.is_empty()
    }
}
impl<K: Eq + Hash + Serialize + DeserializeOwned, V: Serialize> Map<K, V> {
    /// Validate a decoded dependency key before any replay mutates state.
    pub fn validate_key(&self, key: &Key) -> anyhow::Result<()> {
        anyhow::ensure!(key.field == self.label, "journal field mismatch");
        if let Some(value) = &key.key {
            let decoded: K = serde_json::from_value(value.clone())?;
            anyhow::ensure!(
                serde_json::to_value(decoded)? == *value,
                "noncanonical journal key"
            );
        }
        Ok(())
    }
    /// Fingerprint lookup bypassing observation; keys identify the owning field.
    pub fn read_value(&self, key: &Key) -> Value {
        match &key.key {
            None => self.whole(),
            Some(value) => serde_json::from_value::<K>(value.clone())
                .map_or(Value::Null, |decoded| {
                    self.fingerprint(key, self.inner.get(&decoded))
                }),
        }
    }
    pub fn output_value(&self, key: &Key) -> Value {
        match &key.key {
            None => {
                serde_json::to_value(self.inner.iter().collect::<Vec<_>>()).expect("journal output")
            }
            Some(key) => serde_json::from_value::<K>(key.clone()).map_or(Value::Null, |key| {
                let value = self.inner.get(&key);
                serde_json::to_value((value.is_some(), value)).expect("journal output")
            }),
        }
    }
}
impl<K: Eq + Hash + Serialize + DeserializeOwned, V: Serialize + DeserializeOwned> Map<K, V> {
    /// Replay uses raw values, not canonical dependency fingerprints.
    pub fn restore_value(&mut self, key: &Key, value: Value) -> anyhow::Result<()> {
        self.validate_key(key)?;
        if let Some(encoded) = &key.key {
            let decoded = serde_json::from_value::<K>(encoded.clone())?;
            let (present, value): (bool, Value) = serde_json::from_value(value)?;
            if present {
                let value = serde_json::from_value(value)?;
                self.fingerprints
                    .get_mut()
                    .remove(key.encoded.as_ref().unwrap());
                self.inner.insert(decoded, value);
            } else {
                self.fingerprints
                    .get_mut()
                    .remove(key.encoded.as_ref().unwrap());
                self.inner.remove(&decoded);
            }
        } else {
            let entries = serde_json::from_value::<Vec<(K, V)>>(value)?;
            self.fingerprints.get_mut().clear();
            self.inner = entries.into_iter().collect();
        }
        Ok(())
    }
}
impl<K, V, Q> Index<&Q> for Map<K, V>
where
    K: Eq + Hash + Serialize + Borrow<Q>,
    V: Serialize,
    Q: Eq + Hash + Serialize + ?Sized,
{
    type Output = V;
    fn index(&self, key: &Q) -> &Self::Output {
        self.get(key).expect("no entry found for key")
    }
}
impl<K: Eq + Hash + Serialize, V: Serialize> Deref for Map<K, V> {
    type Target = HashMap<K, V>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl<K: Eq + Hash + Serialize, V: Serialize> DerefMut for Map<K, V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.observe_whole();
        if is_active() {
            write(Key::new(self.label, None), self.whole());
        }
        self.fingerprints.get_mut().clear();
        &mut self.inner
    }
}
impl<K: Eq + Hash + Serialize, V: Serialize> Serialize for Map<K, V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.inner.iter().collect::<Vec<_>>().serialize(serializer)
    }
}
impl<'de, K: Eq + Hash + Serialize + Deserialize<'de>, V: Serialize + Deserialize<'de>>
    Deserialize<'de> for Map<K, V>
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self {
            label: "",
            fingerprints: RefCell::new(HashMap::new()),
            inner: Vec::<(K, V)>::deserialize(deserializer)?
                .into_iter()
                .collect(),
        })
    }
}

/// Set variant uses the map journal's presence encoding, so a repeated
/// insertion still records the state that a cached operation must restore.
#[derive(Clone, Debug)]
pub(crate) struct Set<K: Eq + Hash + Serialize> {
    label: &'static str,
    inner: HashSet<K>,
}
impl<K: Eq + Hash + Serialize> Default for Set<K> {
    fn default() -> Self {
        Self::new("")
    }
}
impl<K: Eq + Hash + Serialize> Set<K> {
    pub fn new(label: &'static str) -> Self {
        Self {
            label,
            inner: HashSet::new(),
        }
    }
    #[cfg(test)]
    pub fn raw(&self) -> &HashSet<K> {
        &self.inner
    }
    pub fn into_raw(self) -> HashSet<K> {
        self.inner
    }
    fn key<Q: Serialize + ?Sized>(&self, key: &Q) -> Key {
        Key::new(
            self.label,
            Some(serde_json::to_value(key).expect("set key")),
        )
    }
    fn whole(&self) -> Value {
        let mut entries: Vec<_> = self
            .inner
            .iter()
            .map(|key| {
                let key = serde_json::to_value(key).expect("set key");
                (
                    serde_json::to_string(&key).expect("set key"),
                    Value::Array(vec![key, Value::Null]),
                )
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Value::Array(entries.into_iter().map(|(_, value)| value).collect())
    }
    fn observe_whole(&self) {
        if is_active() {
            read_whole(self.label, self.whole());
        }
    }
    pub fn contains<Q: Hash + Eq + Serialize + ?Sized>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        let present = self.inner.contains(key);
        if is_active() {
            read(self.key(key), entry(present.then_some(&())));
        }
        present
    }
    pub fn insert(&mut self, key: K) -> bool {
        if is_active() {
            let journal_key = self.key(&key);
            let before = entry(self.inner.contains(&key).then_some(&()));
            read(journal_key.clone(), before.clone());
            write(journal_key, before);
        }
        self.inner.insert(key)
    }
    #[cfg(test)]
    pub fn remove<Q: Hash + Eq + Serialize + ?Sized>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        if is_active() {
            let journal_key = self.key(key);
            let before = entry(self.inner.contains(key).then_some(&()));
            read(journal_key.clone(), before.clone());
            write(journal_key, before);
        }
        self.inner.remove(key)
    }
    pub fn extend<I: IntoIterator<Item = K>>(&mut self, values: I) {
        for key in values {
            if is_active() {
                write(
                    self.key(&key),
                    entry(self.inner.contains(&key).then_some(&())),
                );
            }
            self.inner.insert(key);
        }
    }
    pub fn iter(&self) -> std::collections::hash_set::Iter<'_, K> {
        self.observe_whole();
        self.inner.iter()
    }
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.observe_whole();
        self.inner.len()
    }
}
impl<K: Eq + Hash + Serialize + DeserializeOwned> Set<K> {
    /// Validate a decoded dependency key before any replay mutates state.
    pub fn validate_key(&self, key: &Key) -> anyhow::Result<()> {
        anyhow::ensure!(key.field == self.label, "journal field mismatch");
        if let Some(value) = &key.key {
            let decoded: K = serde_json::from_value(value.clone())?;
            anyhow::ensure!(
                serde_json::to_value(decoded)? == *value,
                "noncanonical journal key"
            );
        }
        Ok(())
    }
    pub fn read_value(&self, key: &Key) -> Value {
        match &key.key {
            None => self.whole(),
            Some(key) => serde_json::from_value::<K>(key.clone()).map_or(Value::Null, |key| {
                entry(self.inner.contains(&key).then_some(&()))
            }),
        }
    }
    pub fn output_value(&self, key: &Key) -> Value {
        self.read_value(key)
    }
    pub fn restore_value(&mut self, key: &Key, value: Value) -> anyhow::Result<()> {
        self.validate_key(key)?;
        if let Some(key) = &key.key {
            let key = serde_json::from_value::<K>(key.clone())?;
            let (present, _): (bool, ()) = serde_json::from_value(value)?;
            if present {
                self.inner.insert(key);
            } else {
                self.inner.remove(&key);
            }
        } else {
            self.inner = serde_json::from_value::<Vec<(K, ())>>(value)?
                .into_iter()
                .map(|(key, ())| key)
                .collect();
        }
        Ok(())
    }
}
impl<K: Eq + Hash + Serialize> Deref for Set<K> {
    type Target = HashSet<K>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl<K: Eq + Hash + Serialize> DerefMut for Set<K> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.observe_whole();
        if is_active() {
            write(Key::new(self.label, None), self.whole());
        }
        &mut self.inner
    }
}
impl<K: Eq + Hash + Serialize> FromIterator<K> for Set<K> {
    fn from_iter<I: IntoIterator<Item = K>>(iter: I) -> Self {
        Self {
            label: "",
            inner: iter.into_iter().collect(),
        }
    }
}
impl<K: Eq + Hash + Serialize> Extend<K> for Set<K> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, iter: I) {
        Set::extend(self, iter);
    }
}
impl<K: Eq + Hash + Serialize> IntoIterator for Set<K> {
    type Item = K;
    type IntoIter = std::collections::hash_set::IntoIter<K>;
    fn into_iter(self) -> Self::IntoIter {
        self.observe_whole();
        self.inner.into_iter()
    }
}
impl<'a, K: Eq + Hash + Serialize> IntoIterator for &'a Set<K> {
    type Item = &'a K;
    type IntoIter = std::collections::hash_set::Iter<'a, K>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl<K: Eq + Hash + Serialize> Serialize for Set<K> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.inner.serialize(serializer)
    }
}
impl<'de, K: Eq + Hash + Serialize + Deserialize<'de>> Deserialize<'de> for Set<K> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self {
            label: "",
            inner: HashSet::<K>::deserialize(deserializer)?,
        })
    }
}

pub(crate) fn has_written(key: &Key) -> bool {
    JOURNALS.with(|stack| {
        stack
            .borrow()
            .last()
            .is_some_and(|(_, journal)| own_write(journal, key))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn precheck_journal_records_negative_reads_and_own_writes() {
        let mut map = Map::<String, i32>::new("values");
        let capture = begin();
        assert_eq!(map.get("missing"), None);
        map.insert("new".into(), 3);
        assert_eq!(map.get("new"), Some(&3));
        let journal = capture.finish();
        assert_eq!(journal.reads.len(), 1);
        assert_eq!(journal.touched.len(), 1);
    }
    #[test]
    fn precheck_journal_nested_capture_and_whole_read_preserve_entry_state() {
        let mut map = Map::<String, i32>::new("values");
        map.insert("old".into(), 1);
        let whole = Key::new("values", None);
        let outer_before = map.read_value(&whole);
        let outer = begin();
        map.insert("old".into(), 2);
        let inner_before = map.read_value(&whole);
        let inner = begin();
        map.insert("new".into(), 3);
        let _ = map.values().count();
        let inner = inner.finish();
        let outer = outer.finish();
        let key = Key::new("values", None);
        assert_eq!(outer.reads[&key], outer_before);
        assert_eq!(inner.reads[&key], inner_before);
        assert_eq!(outer.touched.len(), 2);
        assert_eq!(inner.touched.len(), 1);
        assert!(!is_active());
    }
    #[test]
    fn precheck_journal_replays_raw_values_without_observation() {
        let mut source = Map::<String, i32>::new("values");
        let capture = begin();
        source.insert("a".into(), 7);
        let journal = capture.finish();
        let mut target = Map::<String, i32>::new("values");
        let guard = begin();
        for key in journal.touched {
            target
                .restore_value(&key, source.output_value(&key))
                .unwrap();
        }
        assert!(guard.finish().touched.is_empty());
        assert_eq!(target.raw().get("a"), Some(&7));
        let abandoned = begin();
        drop(abandoned);
        assert!(!is_active());
    }
    #[test]
    fn precheck_journal_unit_values_are_distinct_from_absence() {
        let mut map = Map::<String, ()>::new("requests");
        let key = Key::new("requests", Some(Value::String("one".into())));
        assert_eq!(map.read_value(&key), serde_json::json!([false, null]));
        map.insert("one".into(), ());
        assert_eq!(map.output_value(&key), serde_json::json!([true, null]));
        let present = map.read_value(&key);
        assert_eq!(present[0], Value::Bool(true));
        assert!(present[1].is_string());
        let mut restored = Map::<String, ()>::new("requests");
        restored
            .restore_value(&key, map.output_value(&key))
            .unwrap();
        assert!(restored.contains_key("one"));
        map.remove("one");
        restored
            .restore_value(&key, map.output_value(&key))
            .unwrap();
        assert!(!restored.contains_key("one"));
    }
    #[test]
    fn precheck_journal_set_presence_and_repeated_insertion_replay() {
        let mut set = Set::<String>::new("names");
        set.insert("existing".into());
        let capture = begin();
        assert!(!set.contains("missing"));
        assert!(!set.insert("existing".into()));
        assert!(set.contains("existing"));
        set.insert("new".into());
        assert!(set.contains("new"));
        let journal = capture.finish();
        assert_eq!(journal.reads.len(), 3);
        assert_eq!(journal.touched.len(), 2);
        let mut restored = Set::<String>::new("names");
        for key in journal.touched {
            restored
                .restore_value(&key, set.output_value(&key))
                .unwrap();
        }
        assert_eq!(set.raw(), restored.raw());
        let capture = begin();
        set.remove("new");
        let journal = capture.finish();
        for key in journal.touched {
            restored
                .restore_value(&key, set.output_value(&key))
                .unwrap();
        }
        assert_eq!(set.raw(), restored.raw());
    }
    #[test]
    fn precheck_journal_index_reads_and_key_validation() {
        let mut map = Map::<String, i32>::new("values");
        map.insert("one".into(), 1);
        let capture = begin();
        assert_eq!(map["one"], 1);
        let journal = capture.finish();
        assert_eq!(journal.reads.len(), 1);
        let key = Key::new("values", Some(Value::String("one".into())));
        assert!(map.validate_key(&key).is_ok());
        assert!(
            map.validate_key(&Key::new("values", Some(Value::Bool(true))))
                .is_err()
        );
        assert!(
            map.validate_key(&Key::new("other", key.key.clone()))
                .is_err()
        );
        let set = Set::<String>::new("names");
        assert!(set.validate_key(&Key::new("names", None)).is_ok());
        assert!(
            set.validate_key(&Key::new("names", Some(Value::Bool(true))))
                .is_err()
        );
    }
    #[test]
    fn precheck_journal_dispatch_delta_entries_scale_with_writes() {
        for count in [8, 32, 128] {
            let capture = begin();
            for index in 0..count {
                let key = Key::new("symbols", Some(Value::String(format!("C{index}"))));
                write(key.clone(), Value::Null);
                let dirty = take_dirty_symbols();
                assert_eq!(dirty, vec![key]);
                record_dispatch(DispatchEvent {
                    writes: dirty
                        .into_iter()
                        .map(|key| (key, Value::Bool(true)))
                        .collect(),
                    read: Value::String(format!("C{index}.method")),
                    result: true,
                });
                // Repeated reads share the previous overlay without another
                // cumulative snapshot of all earlier class declarations.
                record_dispatch(DispatchEvent {
                    writes: take_dirty_symbols()
                        .into_iter()
                        .map(|key| (key, Value::Null))
                        .collect(),
                    read: Value::String(format!("C{index}.method")),
                    result: true,
                });
            }
            let journal = capture.finish();
            assert_eq!(journal.dispatch.len(), count * 2);
            assert_eq!(
                journal
                    .dispatch
                    .iter()
                    .map(|event| event.writes.len())
                    .sum::<usize>(),
                count
            );
            let wire = serde_json::to_value(&journal.dispatch).unwrap();
            let decoded: Vec<DispatchEvent> = serde_json::from_value(wire).unwrap();
            assert_eq!(decoded.len(), count * 2);
        }
    }
    #[derive(Clone, Deserialize)]
    struct CountedValue(Vec<u64>);
    thread_local! {
        static MAP_SERIALIZATION_WORK: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    }
    impl Serialize for CountedValue {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            MAP_SERIALIZATION_WORK.with(|work| {
                let (calls, elements) = work.get();
                work.set((calls + 1, elements + self.0.len()));
            });
            self.0.serialize(serializer)
        }
    }
    #[test]
    fn precheck_journal_map_fingerprints_serialize_once_per_value_epoch() {
        for readers in [8, 32, 128] {
            for width in [8, 32, 128] {
                MAP_SERIALIZATION_WORK.with(|work| work.set((0, 0)));
                let assert_work = |calls| {
                    assert_eq!(
                        MAP_SERIALIZATION_WORK.with(|work| work.get()),
                        (calls, calls * width),
                        "readers={readers}, width={width}"
                    );
                };
                let mut map = Map::<String, CountedValue>::new("templates");
                map.insert("shared".into(), CountedValue(vec![1; width]));
                assert_work(0); // No capture: insertion does not serialize a value.
                let key = Key::new("templates", Some(Value::String("shared".into())));
                let mut dependency_bytes = 0;
                for _ in 0..readers {
                    let capture = begin();
                    assert_eq!(map.get("shared").unwrap().0.len(), width);
                    let journal = capture.finish();
                    assert_eq!(journal.reads.len(), 1);
                    let recorded = &journal.reads[&key];
                    assert_eq!(*recorded, map.read_value(&key));
                    dependency_bytes += serde_json::to_vec(recorded).unwrap().len();
                }
                assert_work(1);
                assert!(dependency_bytes <= readers * 80);
                let whole_key = Key::new("templates", None);
                assert_eq!(map.read_value(&whole_key).as_array().unwrap().len(), 1);
                assert_work(1);

                map.insert("shared".into(), CountedValue(vec![2; width]));
                let inserted = map.read_value(&key);
                assert_work(2);
                map.entry("shared".into())
                    .and_modify(|value| value.0[0] = 3);
                assert_ne!(map.read_value(&key), inserted);
                assert_work(3);
                map.get_mut("shared").unwrap().0[0] = 4; // DerefMut invalidates.
                let original = map.read_value(&key);
                assert_work(4);
                let mut cloned = map.clone();
                assert_eq!(cloned.read_value(&key), original);
                cloned.insert("shared".into(), CountedValue(vec![5; width]));
                assert_ne!(cloned.read_value(&key), original);
                assert_eq!(map.read_value(&key), original);
                assert_work(5); // Divergent cloned maps have independent memos.

                map.restore_value(&key, serde_json::json!([true, vec![6; width]]))
                    .unwrap();
                map.read_value(&key);
                assert_work(6);
                map.restore_value(&whole_key, serde_json::json!([["shared", vec![7; width]]]))
                    .unwrap();
                map.read_value(&key);
                assert_work(7);
                map.remove("shared");
                assert_eq!(map.read_value(&key), serde_json::json!([false, null]));
                assert_work(7);
                map.insert("shared".into(), CountedValue(vec![8; width]));
                map.read_value(&key);
                assert_work(8); // Negative fingerprints invalidate on insertion.
                let raw = map.output_value(&key);
                assert_eq!(raw[1].as_array().unwrap().len(), width);
                assert_work(9); // Replay payloads intentionally remain complete.
            }
        }
    }
}
