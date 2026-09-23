//! Mapping store: `payload_id` → recorded replacements, needed for demasking.
//!
//! Redis is the shared store for several replicas; process memory is the L1
//! cache in front of it. A mapping reaches the cache only after Redis has
//! accepted it, and a Redis error is always returned to the caller: a mapping
//! that lives in the memory of one replica only would make the other replicas
//! answer with a mask where the caller expects the original.
//!
//! Memory is the whole store in exactly two cases: `store.backend: memory`,
//! and — with `store.memory_fallback: true` — a replica whose Redis was
//! unreachable at startup, which then runs and reports itself as degraded.

pub mod crypto;
pub mod memory;
pub mod redis;
#[cfg(test)]
pub mod testing;

use async_trait::async_trait;
use maskarad_core::MaskEntry;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mapping {
    pub system: String,
    /// SHA-256 of the original text (to recognise a retried mask request).
    pub original_hash: String,
    /// SHA-256 of the masked text (to recognise the exact demask request).
    pub masked_hash: String,
    pub entries: Vec<MaskEntry>,
    pub created_at: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    Unavailable(String),
    #[error("stored record is corrupt or encrypted with another key")]
    Corrupt,
    /// A record for this key already exists; writes never overwrite. For the
    /// caller this is "already there", not a failure.
    #[error("a mapping for this key already exists")]
    Exists,
}

#[async_trait]
pub trait MappingStore: Send + Sync {
    async fn put(&self, key: &str, mapping: &Mapping) -> Result<(), StoreError>;
    async fn get(&self, key: &str) -> Result<Option<Arc<Mapping>>, StoreError>;
    async fn healthy(&self) -> bool;
    fn backend(&self) -> &'static str;
}

pub fn sha256_hex(data: &str) -> String {
    Sha256::digest(data.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The payload id itself is never stored: the key is its hash.
pub fn mapping_key(system: &str, payload_id: &str) -> String {
    format!("mk:{system}:{}", sha256_hex(payload_id))
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The primary store (Redis) behind a memory L1 cache, or memory alone.
pub struct TieredStore {
    primary: Option<Arc<dyn MappingStore>>,
    cache: Arc<memory::MemoryStore>,
    degraded: AtomicBool,
}

impl TieredStore {
    /// The memory backend: mappings never leave the process.
    pub fn memory_only(ttl: Duration, capacity: u64) -> Self {
        Self {
            primary: None,
            cache: Arc::new(memory::MemoryStore::new(ttl, capacity)),
            degraded: AtomicBool::new(false),
        }
    }

    /// Memory only because Redis was unreachable at startup: the replica works,
    /// but reports itself as degraded — its mappings are invisible to the
    /// other replicas.
    pub fn startup_fallback(ttl: Duration, capacity: u64) -> Self {
        let store = Self::memory_only(ttl, capacity);
        store.set_degraded(true);
        store
    }

    pub fn with_primary(primary: Arc<dyn MappingStore>, ttl: Duration, capacity: u64) -> Self {
        Self {
            primary: Some(primary),
            cache: Arc::new(memory::MemoryStore::new(ttl, capacity)),
            degraded: AtomicBool::new(false),
        }
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
    }

    fn set_degraded(&self, value: bool) {
        if self.degraded.swap(value, Ordering::Relaxed) != value {
            if value {
                tracing::warn!("mapping store degraded: the primary store is failing");
            } else {
                tracing::info!("mapping store recovered");
            }
            metrics::gauge!("maskarad_store_degraded").set(if value { 1.0 } else { 0.0 });
        }
    }
}

#[async_trait]
impl MappingStore for TieredStore {
    /// The primary store first; the L1 cache only holds what the primary has
    /// accepted, so a failed write leaves no trace on this replica either.
    /// A record that already exists in the primary is kept as it is (another
    /// replica or a retried write got there first) and is not cached here:
    /// the next read fetches the authoritative copy.
    async fn put(&self, key: &str, mapping: &Mapping) -> Result<(), StoreError> {
        if let Some(primary) = &self.primary {
            match primary.put(key, mapping).await {
                Ok(()) => {
                    self.set_degraded(false);
                    metrics::counter!("maskarad_store_ops_total", "op" => "put", "result" => "ok")
                        .increment(1);
                }
                Err(StoreError::Exists) => {
                    self.set_degraded(false);
                    metrics::counter!("maskarad_store_ops_total", "op" => "put", "result" => "exists").increment(1);
                    return Ok(());
                }
                Err(e) => {
                    metrics::counter!("maskarad_store_ops_total", "op" => "put", "result" => "error").increment(1);
                    self.set_degraded(true);
                    return Err(e);
                }
            }
        }
        self.cache.put(key, mapping).await
    }

    async fn get(&self, key: &str) -> Result<Option<Arc<Mapping>>, StoreError> {
        if let Some(m) = self.cache.get(key).await? {
            metrics::counter!("maskarad_store_ops_total", "op" => "get", "result" => "cache_hit")
                .increment(1);
            return Ok(Some(m));
        }
        let Some(primary) = &self.primary else {
            return Ok(None);
        };
        match primary.get(key).await {
            Ok(found) => {
                self.set_degraded(false);
                metrics::counter!("maskarad_store_ops_total", "op" => "get", "result" => if found.is_some() { "ok" } else { "miss" }).increment(1);
                if let Some(m) = &found {
                    self.cache.put(key, m).await?;
                }
                Ok(found)
            }
            Err(e) => {
                metrics::counter!("maskarad_store_ops_total", "op" => "get", "result" => "error")
                    .increment(1);
                self.set_degraded(true);
                Err(e)
            }
        }
    }

    async fn healthy(&self) -> bool {
        match &self.primary {
            Some(p) => p.healthy().await,
            None => true,
        }
    }

    fn backend(&self) -> &'static str {
        match &self.primary {
            Some(p) => p.backend(),
            None => "memory",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{sample_mapping, Failure, FlakyStore};
    use super::*;

    const TTL: Duration = Duration::from_secs(60);
    const CAPACITY: u64 = 1 << 20;

    fn tiered() -> (Arc<FlakyStore>, TieredStore) {
        let primary = Arc::new(FlakyStore::new());
        let store = TieredStore::with_primary(primary.clone(), TTL, CAPACITY);
        (primary, store)
    }

    #[tokio::test]
    async fn failed_primary_put_is_returned_and_leaves_no_cached_copy() {
        let (primary, store) = tiered();
        primary.fail_put(Failure::Unavailable);
        let err = store.put("k", &sample_mapping(1)).await.unwrap_err();
        assert!(matches!(err, StoreError::Unavailable(_)), "{err}");
        assert!(store.is_degraded());

        // Redis is back: neither it nor this replica's cache knows the key.
        primary.fail_put(Failure::None);
        assert!(store.get("k").await.unwrap().is_none());
        assert_eq!(primary.gets(), 1, "the miss went to the primary");
        assert!(!store.is_degraded());
    }

    #[tokio::test]
    async fn successful_put_reaches_primary_then_cache() {
        let (primary, store) = tiered();
        store.put("k", &sample_mapping(2)).await.unwrap();
        assert_eq!(primary.puts(), 1);

        // Served from L1: the primary is not consulted even while it fails.
        primary.fail_get(true);
        let found = store.get("k").await.unwrap().expect("cached");
        assert_eq!(found.entries.len(), 2);
        assert_eq!(primary.gets(), 0);
    }

    #[tokio::test]
    async fn existing_record_in_primary_is_kept_and_not_cached_here() {
        let (primary, store) = tiered();
        primary.put("k", &sample_mapping(3)).await.unwrap();
        primary.fail_put(Failure::Exists);

        store.put("k", &sample_mapping(1)).await.unwrap();
        assert!(!store.is_degraded());
        assert_eq!(primary.puts(), 2);

        // The authoritative copy comes from the primary, not our rejected write.
        let found = store.get("k").await.unwrap().expect("present");
        assert_eq!(found.entries.len(), 3);
        assert_eq!(primary.gets(), 1);
    }

    #[tokio::test]
    async fn primary_get_error_is_returned() {
        let (primary, store) = tiered();
        primary.fail_get(true);
        let err = store.get("k").await.unwrap_err();
        assert!(matches!(err, StoreError::Unavailable(_)), "{err}");
        assert!(store.is_degraded());
    }

    #[tokio::test]
    async fn primary_hit_fills_cache() {
        let (primary, store) = tiered();
        primary.put("k", &sample_mapping(1)).await.unwrap();
        assert!(store.get("k").await.unwrap().is_some());
        primary.fail_get(true);
        assert!(store.get("k").await.unwrap().is_some());
        assert_eq!(primary.gets(), 1);
    }

    #[tokio::test]
    async fn memory_only_stores_and_is_not_degraded() {
        let store = TieredStore::memory_only(TTL, CAPACITY);
        store.put("k", &sample_mapping(1)).await.unwrap();
        assert!(store.get("k").await.unwrap().is_some());
        assert!(!store.is_degraded());
        assert_eq!(store.backend(), "memory");
    }

    #[tokio::test]
    async fn startup_fallback_is_degraded_from_the_start() {
        let store = TieredStore::startup_fallback(TTL, CAPACITY);
        assert!(store.is_degraded());
        store.put("k", &sample_mapping(1)).await.unwrap();
        assert!(store.is_degraded(), "no primary can ever clear it");
    }
}
