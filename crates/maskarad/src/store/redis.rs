//! Redis backend: one multiplexed, auto-reconnecting connection per process,
//! values sealed with AES-256-GCM, TTL on every key.
//!
//! A mapping is written with `SET key value NX EX ttl`: the first writer for
//! a payload_id wins and a concurrent retry (or a write whose first attempt
//! timed out after Redis had applied it) sees [`StoreError::Exists`] rather
//! than overwriting the record. Every command runs under the configured
//! timeout and is retried once after a timeout; other errors are final.

use super::crypto::Cipher;
use super::{Mapping, MappingStore, StoreError};
use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

pub struct RedisStore {
    conn: ConnectionManager,
    cipher: Cipher,
    ttl: Duration,
    timeout: Duration,
}

impl RedisStore {
    pub async fn connect(
        url: &str,
        cipher: Cipher,
        ttl: Duration,
        timeout: Duration,
    ) -> Result<Self, StoreError> {
        let client = redis::Client::open(url)
            .map_err(|e| StoreError::Unavailable(format!("invalid redis url: {e}")))?;
        let conn = tokio::time::timeout(Duration::from_secs(5), ConnectionManager::new(client))
            .await
            .map_err(|_| StoreError::Unavailable("redis connect timeout".into()))?
            .map_err(|e| StoreError::Unavailable(format!("redis connect failed: {e}")))?;
        Ok(Self {
            conn,
            cipher,
            ttl,
            timeout,
        })
    }
}

/// `SET key value NX EX ttl_seconds`: create only, with a TTL.
fn set_nx_ex(key: &str, value: &[u8], ttl_seconds: u64) -> redis::Cmd {
    let mut cmd = redis::cmd("SET");
    cmd.arg(key).arg(value).arg("NX").arg("EX").arg(ttl_seconds);
    cmd
}

/// Runs `op` under `timeout`. A timeout is retried once with a fresh future;
/// a second timeout, or any Redis error, is returned as `Unavailable`.
async fn retry_once_on_timeout<T, F, Fut>(
    op_name: &'static str,
    timeout: Duration,
    mut op: F,
) -> Result<T, StoreError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = redis::RedisResult<T>>,
{
    let mut retried = false;
    loop {
        match tokio::time::timeout(timeout, op()).await {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(e)) => return Err(StoreError::Unavailable(format!("redis error: {e}"))),
            Err(_) if !retried => {
                retried = true;
                metrics::counter!("maskarad_store_retries_total", "op" => op_name).increment(1);
                tracing::debug!(op = op_name, "redis timeout, retrying once");
            }
            Err(_) => return Err(StoreError::Unavailable("redis timeout".into())),
        }
    }
}

#[async_trait]
impl MappingStore for RedisStore {
    async fn put(&self, key: &str, mapping: &Mapping) -> Result<(), StoreError> {
        let plain = serde_json::to_vec(mapping).map_err(|_| StoreError::Corrupt)?;
        let sealed = self.cipher.seal(&plain);
        let ttl = self.ttl.as_secs();
        let reply: Option<String> = retry_once_on_timeout("put", self.timeout, || {
            let mut conn = self.conn.clone();
            let cmd = set_nx_ex(key, &sealed, ttl);
            async move { cmd.query_async(&mut conn).await }
        })
        .await?;
        // `SET … NX` answers OK when the key was created and nil when it exists.
        match reply {
            Some(_) => Ok(()),
            None => Err(StoreError::Exists),
        }
    }

    async fn get(&self, key: &str) -> Result<Option<Arc<Mapping>>, StoreError> {
        let data: Option<Vec<u8>> = retry_once_on_timeout("get", self.timeout, || {
            let mut conn = self.conn.clone();
            async move { conn.get(key).await }
        })
        .await?;
        match data {
            None => Ok(None),
            Some(sealed) => {
                let plain = self.cipher.open(&sealed).ok_or(StoreError::Corrupt)?;
                let mapping: Mapping =
                    serde_json::from_slice(&plain).map_err(|_| StoreError::Corrupt)?;
                Ok(Some(Arc::new(mapping)))
            }
        }
    }

    async fn healthy(&self) -> bool {
        let mut conn = self.conn.clone();
        let cmd = redis::cmd("PING");
        let fut = cmd.query_async::<String>(&mut conn);
        matches!(tokio::time::timeout(self.timeout, fut).await, Ok(Ok(_)))
    }

    fn backend(&self) -> &'static str {
        "redis"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TIMEOUT: Duration = Duration::from_millis(20);

    #[test]
    fn put_is_a_create_only_set_with_ttl() {
        let packed = set_nx_ex("mk:sys:abc", b"sealed", 3600).get_packed_command();
        assert_eq!(
            packed,
            b"*6\r\n$3\r\nSET\r\n$10\r\nmk:sys:abc\r\n$6\r\nsealed\r\n$2\r\nNX\r\n$2\r\nEX\r\n$4\r\n3600\r\n"
        );
    }

    /// `op` hangs on the attempts listed in `hang_on` and answers `reply`
    /// otherwise; returns how many times it was called.
    async fn run(
        hang_on: &[usize],
        reply: redis::RedisResult<u8>,
    ) -> (Result<u8, StoreError>, usize) {
        let calls = AtomicUsize::new(0);
        let result = retry_once_on_timeout("test", TIMEOUT, || {
            let attempt = calls.fetch_add(1, Ordering::SeqCst);
            let hang = hang_on.contains(&attempt);
            let reply = reply
                .as_ref()
                .map(|v| *v)
                .map_err(|e| redis::RedisError::from((e.kind(), "test error", e.to_string())));
            async move {
                if hang {
                    std::future::pending::<()>().await;
                }
                reply
            }
        })
        .await;
        (result, calls.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn success_needs_one_attempt() {
        let (result, calls) = run(&[], Ok(7)).await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn one_timeout_is_retried_and_the_retry_answers() {
        let (result, calls) = run(&[0], Ok(7)).await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn two_timeouts_give_up() {
        let (result, calls) = run(&[0, 1], Ok(7)).await;
        assert!(
            matches!(&result, Err(StoreError::Unavailable(m)) if m == "redis timeout"),
            "{result:?}"
        );
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn a_redis_error_is_not_retried() {
        let err = redis::RedisError::from((redis::ErrorKind::IoError, "broken pipe"));
        let (result, calls) = run(&[], Err(err)).await;
        assert!(
            matches!(&result, Err(StoreError::Unavailable(m)) if m.starts_with("redis error:")),
            "{result:?}"
        );
        assert_eq!(calls, 1);
    }

    /// Against a live Redis when `MASKARAD_TEST_REDIS_URL` is set (for example
    /// `redis://127.0.0.1:6379/0`); a no-op otherwise.
    #[tokio::test]
    async fn live_redis_writes_are_create_only() {
        let Ok(url) = std::env::var("MASKARAD_TEST_REDIS_URL") else {
            return;
        };
        let store = RedisStore::connect(
            &url,
            Cipher::from_secret("test key"),
            Duration::from_secs(60),
            Duration::from_secs(1),
        )
        .await
        .expect("connect");
        let key = format!(
            "mk:test:{}",
            super::super::sha256_hex(&crate::logging::request_id())
        );

        store
            .put(&key, &super::super::testing::sample_mapping(2))
            .await
            .unwrap();
        let second = store
            .put(&key, &super::super::testing::sample_mapping(5))
            .await;
        assert!(matches!(second, Err(StoreError::Exists)), "{second:?}");

        let found = store.get(&key).await.unwrap().expect("present");
        assert_eq!(found.entries.len(), 2, "the first record stays");
        assert!(store
            .get(&format!("{key}:missing"))
            .await
            .unwrap()
            .is_none());
        assert!(store.healthy().await);

        let mut conn = store.conn.clone();
        let _: () = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut conn)
            .await
            .unwrap();
    }
}
