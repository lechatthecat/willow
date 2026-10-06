//! Folded-multiply hasher for runtime-generated integer keys.
//!
//! Task ids, wait tickets and heap addresses are produced by the runtime, not
//! by program input, so SipHash's flooding resistance buys nothing and costs a
//! keyed multi-round hash per lookup on scheduler and collector hot paths
//! (willow-8hq4.16, willow-8hq4.18, willow-lk92).

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

/// One widening `mul` whose two halves are xored, so every input bit reaches
/// both the low and the high output bits. hashbrown indexes buckets with the
/// low hash bits and tags with the top seven; sharded tables whose keys share
/// their low bits (task ids per shard, aligned addresses) still spread.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct IdHasher(u64);

impl Hasher for IdHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }

    fn write_u64(&mut self, value: u64) {
        let product = u128::from(self.0 ^ value) * 0x9e37_79b9_7f4a_7c15;
        self.0 = (product as u64) ^ ((product >> 64) as u64);
    }

    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

pub(crate) type IdBuildHasher = BuildHasherDefault<IdHasher>;
pub(crate) type IdMap<K, V> = HashMap<K, V, IdBuildHasher>;
pub(crate) type IdSet<K> = HashSet<K, IdBuildHasher>;

/// Hash one integer key, e.g. to pick a lock shard.
pub(crate) fn hash_u64(value: u64) -> u64 {
    let mut hasher = IdHasher::default();
    hasher.write_u64(value);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{BuildHasher, Hash};

    fn hash_of<T: Hash>(value: T) -> u64 {
        IdBuildHasher::default().hash_one(value)
    }

    #[test]
    fn integer_widths_hash_like_their_u64_value() {
        assert_eq!(hash_of(7_u32), hash_u64(7));
        assert_eq!(hash_of(7_usize), hash_u64(7));
        assert_eq!(hash_of(7_u64), hash_u64(7));
    }

    #[test]
    fn aligned_and_strided_keys_spread_over_low_bucket_bits_and_top_tags() {
        // 4 KiB-aligned addresses and shard-strided task ids share their low
        // bits; both hash halves must still vary.
        for stride in [4096_u64, 256] {
            let keys = (1..=1024_u64).map(|n| n * stride);
            let low: HashSet<u64> = keys.clone().map(|k| hash_u64(k) & 0x3ff).collect();
            let top: HashSet<u64> = keys.map(|k| hash_u64(k) >> 57).collect();
            assert!(
                low.len() > 512,
                "stride {stride}: {} low buckets",
                low.len()
            );
            assert_eq!(top.len(), 128, "stride {stride}: top-7 tags");
        }
    }

    #[test]
    fn id_map_round_trips_entries() {
        let mut map: IdMap<u64, u64> = (0..1000).map(|k| (k * 256, k)).collect();
        assert_eq!(map.len(), 1000);
        assert_eq!(map.remove(&(999 * 256)), Some(999));
        assert!(!map.contains_key(&(999 * 256)));
    }
}
