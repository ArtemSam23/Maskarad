//! In-process store with TTL, used as the memory backend and as the L1 cache.
//!
//! The cache is bounded by an estimate of the bytes it holds, not by the
//! number of records: mappings differ in size by orders of magnitude (one
//! entity versus a 100k-token document with thousands), and a count limit
//! let the process grow past its memory limit.

use super::{Mapping, MappingStore, StoreError};
use async_trait::async_trait;
use maskarad_core::MaskEntry;
use moka::future::Cache;
use std::mem::size_of;
use std::sync::Arc;
use std::time::Duration;

pub struct MemoryStore {
    cache: Cache<String, Arc<Mapping>>,
}

/// Approximate heap footprint of one cached record: the key, the mapping
/// struct with its strings, and every entry with its mask and original.
pub fn weight(key: &str, mapping: &Mapping) -> u32 {
    let entries: usize = mapping
        .entries
        .iter()
        .map(|e| {
            size_of::<MaskEntry>() + e.ty.as_str().len() + e.mask.len() + e.original.expose().len()
        })
        .sum();
    let total = size_of::<String>()
        + key.len()
        + size_of::<Arc<Mapping>>()
        + size_of::<Mapping>()
        + mapping.system.len()
        + mapping.original_hash.len()
        + mapping.masked_hash.len()
        + entries;
    u32::try_from(total).unwrap_or(u32::MAX)
}

impl MemoryStore {
    /// `capacity_bytes` bounds the estimated size of all records together; a
    /// record larger than the whole capacity is not admitted.
    pub fn new(ttl: Duration, capacity_bytes: u64) -> Self {
        Self {
            cache: Cache::builder()
                .time_to_live(ttl)
                .max_capacity(capacity_bytes)
                .weigher(|key: &String, mapping: &Arc<Mapping>| weight(key, mapping))
                .build(),
        }
    }

    #[cfg(test)]
    async fn settled_size(&self) -> (u64, u64) {
        self.cache.run_pending_tasks().await;
        (self.cache.entry_count(), self.cache.weighted_size())
    }
}

#[async_trait]
impl MappingStore for MemoryStore {
    async fn put(&self, key: &str, mapping: &Mapping) -> Result<(), StoreError> {
        self.cache
            .insert(key.to_string(), Arc::new(mapping.clone()))
            .await;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<Arc<Mapping>>, StoreError> {
        Ok(self.cache.get(key).await)
    }

    async fn healthy(&self) -> bool {
        true
    }

    fn backend(&self) -> &'static str {
        "memory"
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::sample_mapping;
    use super::*;

    const TTL: Duration = Duration::from_secs(60);

    #[test]
    fn weight_counts_every_entry_and_its_original() {
        let small = weight("k", &sample_mapping(1));
        let large = weight("k", &sample_mapping(1000));
        assert!(small > 0);
        // Each entry adds its struct, mask and original: well over 40 bytes.
        assert!(large - small > 999 * 40, "{small} -> {large}");
        assert!(weight("a much longer key", &sample_mapping(1)) > small);
    }

    #[tokio::test]
    async fn total_bytes_stay_within_capacity() {
        let mapping = sample_mapping(20);
        let one = u64::from(weight("k0000", &mapping));
        let capacity = one * 3;
        let store = MemoryStore::new(TTL, capacity);
        for i in 0..50 {
            store.put(&format!("k{i:04}"), &mapping).await.unwrap();
        }
        let (entries, bytes) = store.settled_size().await;
        assert!(
            bytes <= capacity,
            "{bytes} bytes cached, capacity {capacity}"
        );
        assert!((1..=3).contains(&entries), "{entries} entries");
    }

    #[tokio::test]
    async fn a_record_larger_than_the_capacity_is_not_kept() {
        let store = MemoryStore::new(TTL, 512);
        store.put("huge", &sample_mapping(100)).await.unwrap();
        let (entries, bytes) = store.settled_size().await;
        assert_eq!((entries, bytes), (0, 0));
        assert!(store.get("huge").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn small_records_fit_and_are_served() {
        let store = MemoryStore::new(TTL, 1 << 20);
        store.put("k", &sample_mapping(2)).await.unwrap();
        assert_eq!(store.get("k").await.unwrap().unwrap().entries.len(), 2);
    }
}
