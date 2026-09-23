//! Test double for the primary store: a memory store whose `put`/`get` can be
//! switched to fail, with call counters to assert what the tiered store did.

use super::memory::MemoryStore;
use super::{Mapping, MappingStore, StoreError};
use async_trait::async_trait;
use maskarad_core::{MaskEntry, PiiType};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    None,
    Unavailable,
    Exists,
}

pub struct FlakyStore {
    inner: MemoryStore,
    put_failure: std::sync::Mutex<Failure>,
    fail_get: AtomicBool,
    hang_get: AtomicBool,
    pub puts: AtomicUsize,
    pub gets: AtomicUsize,
}

impl Default for FlakyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl FlakyStore {
    pub fn new() -> Self {
        Self {
            inner: MemoryStore::new(Duration::from_secs(60), 1 << 20),
            put_failure: std::sync::Mutex::new(Failure::None),
            fail_get: AtomicBool::new(false),
            hang_get: AtomicBool::new(false),
            puts: AtomicUsize::new(0),
            gets: AtomicUsize::new(0),
        }
    }

    pub fn fail_put(&self, failure: Failure) {
        *self.put_failure.lock().expect("test mutex") = failure;
    }

    pub fn fail_get(&self, fail: bool) {
        self.fail_get.store(fail, Ordering::SeqCst);
    }

    /// `get` never completes, like a store that stopped answering.
    pub fn hang_get(&self, hang: bool) {
        self.hang_get.store(hang, Ordering::SeqCst);
    }

    pub fn puts(&self) -> usize {
        self.puts.load(Ordering::SeqCst)
    }

    pub fn gets(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl MappingStore for FlakyStore {
    async fn put(&self, key: &str, mapping: &Mapping) -> Result<(), StoreError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        let failure = *self.put_failure.lock().expect("test mutex");
        match failure {
            Failure::None => self.inner.put(key, mapping).await,
            Failure::Unavailable => Err(StoreError::Unavailable("redis timeout".into())),
            Failure::Exists => Err(StoreError::Exists),
        }
    }

    async fn get(&self, key: &str) -> Result<Option<Arc<Mapping>>, StoreError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        if self.hang_get.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if self.fail_get.load(Ordering::SeqCst) {
            return Err(StoreError::Unavailable("redis timeout".into()));
        }
        self.inner.get(key).await
    }

    async fn healthy(&self) -> bool {
        true
    }

    fn backend(&self) -> &'static str {
        "flaky"
    }
}

/// A mapping with `n` entries of realistic size.
pub fn sample_mapping(n: usize) -> Mapping {
    let entries = (0..n)
        .map(|i| MaskEntry {
            ty: PiiType::new("PASSPORT"),
            start: i * 20,
            end: i * 20 + 11,
            masked_start: i * 20,
            masked_end: i * 20 + 11,
            mask: "45** ****56".into(),
            original: "4509 123456".to_string().into(),
        })
        .collect();
    Mapping {
        system: "alfasonar".into(),
        original_hash: "a".repeat(64),
        masked_hash: "b".repeat(64),
        entries,
        created_at: 0,
    }
}
