//! Revision-aware ownership for AI query values. Uses the same QueryTable and
//! BodyId as other CompilerDb query families; it is not a second evaluator.
use super::{
    ids::BodyId,
    query::{QueryStats, QueryTable},
};
use anyhow::Result;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Symbol,
    References,
    Effects,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Key(pub BodyId, pub Kind);
pub(crate) struct AnalysisQueries {
    values: QueryTable<Key, Value>,
    sizes: HashMap<Key, usize>,
    order: VecDeque<Key>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}
impl AnalysisQueries {
    pub(crate) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            values: QueryTable::named("analysis_body"),
            sizes: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            max_entries,
            max_bytes,
        }
    }
    pub(crate) fn invalidate(&mut self, invalid: &HashSet<Key>) {
        self.values.retain(|key| !invalid.contains(key));
        self.sizes.retain(|key, size| {
            if invalid.contains(key) {
                self.bytes -= *size;
                false
            } else {
                true
            }
        });
        self.order.retain(|key| !invalid.contains(key));
    }
    pub(crate) fn query(
        &mut self,
        key: Key,
        compute: impl FnOnce() -> Result<Value>,
    ) -> Result<Arc<Value>> {
        let hit = self.values.is_ready(&key);
        let value = self.values.query(key, compute)?;
        if !hit {
            struct Size(usize);
            impl std::io::Write for Size {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    self.0 += bytes.len();
                    Ok(bytes.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let mut size = Size(0);
            serde_json::to_writer(&mut size, &*value)?;
            let size = size.0;
            if size > self.max_bytes || self.max_entries == 0 {
                self.values.remove(&key);
                return Ok(value);
            }

            while self.sizes.len() >= self.max_entries || self.bytes + size > self.max_bytes {
                let old = self.order.pop_front().expect("cache size bookkeeping");
                self.bytes -= self.sizes.remove(&old).unwrap();
                self.values.remove(&old);
            }
            self.sizes.insert(key, size);
            self.order.push_back(key);
            self.bytes += size;
        }
        Ok(value)
    }
    pub(crate) fn stats(&self) -> QueryStats {
        self.values.stats()
    }
    pub(crate) fn retained(&self) -> (usize, usize) {
        (self.sizes.len(), self.bytes)
    }
}
