//! 账号/凭据/代理隔离的官方 client 池（TLS-CTD-001 §14 / FR-016）。
//!
//! pool key = `(account_key, credential_revision, proxy_digest, route_class)`。
//! 连接池按 `(account, revision, proxy, route_class)` 隔离；Cloudflare cookie store
//! 是官方设计的进程级共享 jar，但只允许 CF 基础设施 cookie 名（`cf_clearance`、
//! `__cf_bm` 等），账号/session cookie 一律被过滤，共享面不含账号凭据。
//! 旧 credential revision 的条目按 TTL/容量淘汰。

use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::Mutex;

use super::proxy::ProxyDigest;
use crate::contract::operation::Operation;
use crate::limits::CLIENT_POOL_IDLE_TTL_SECONDS;
use crate::limits::CLIENT_POOL_MAX_ENTRIES;
use crate::observability::Metrics;
use std::sync::Arc;

/// 池隔离键（Debug 不含 proxy credential）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientPoolKey {
    pub account_key: String,
    pub credential_revision: String,
    pub proxy_digest: ProxyDigest,
    pub route_class: &'static str,
}

impl ClientPoolKey {
    pub fn new(
        account_key: &str,
        credential_revision: &str,
        proxy_digest: ProxyDigest,
        route_class: &'static str,
    ) -> Self {
        Self {
            account_key: account_key.to_string(),
            credential_revision: credential_revision.to_string(),
            proxy_digest,
            route_class,
        }
    }
}

/// 池条目：官方 reqwest client + 最后使用时间。
struct PoolEntry {
    client: reqwest::Client,
    last_used: Instant,
}

/// 有界 client pool（单进程；容量/Idle TTL 静态上限）。
pub struct ClientPool {
    inner: Mutex<HashMap<ClientPoolKey, PoolEntry>>,
    metrics: Arc<Metrics>,
}

impl ClientPool {
    pub fn new(metrics: Arc<Metrics>) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            metrics,
        }
    }

    /// 获取（或构建）指定 pool key 的官方 reqwest client。
    ///
    /// `build` 闭包负责用官方 client factory 构造 client；pool 只负责
    /// 隔离、TTL 与容量。client 构建失败不缓存。
    pub async fn get_or_insert_with(
        &self,
        key: ClientPoolKey,
        build: impl FnOnce() -> Result<reqwest::Client, String>,
    ) -> Result<reqwest::Client, String> {
        let mut guard = self.inner.lock().await;
        if let Some(entry) = guard.get_mut(&key) {
            if entry.last_used.elapsed().as_secs() <= CLIENT_POOL_IDLE_TTL_SECONDS {
                // F-014：cache hit 必须刷新 last_used——TTL 是"空闲"而非
                // "创建后"；活跃 client 不再周期性重建。
                entry.last_used = Instant::now();
                return Ok(entry.client.clone());
            }
            // 过期条目淘汰。
            guard.remove(&key);
            self.metrics.pool_evictions_total.increment("idle_ttl");
        }

        // 容量淘汰：超出上限时优先淘汰最久未用的条目。
        if guard.len() >= CLIENT_POOL_MAX_ENTRIES
            && let Some(oldest_key) = guard
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
        {
            guard.remove(&oldest_key);
            self.metrics.pool_evictions_total.increment("capacity");
        }

        let client = build()?;
        guard.insert(
            key,
            PoolEntry {
                client: client.clone(),
                last_used: Instant::now(),
            },
        );
        self.metrics
            .pool_entries
            .store(guard.len() as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(client)
    }

    /// 主动淘汰指定账号的 credential revision 条目（token 轮换/drain；
    /// F-014：按 account_key + revision 作用域，不横跨所有账号）。
    /// P3-5：当前无生产调用方（token 轮换路径尚未接入），保留为池
    /// 管理 API；接入时删除 allow。
    #[allow(dead_code)]
    pub async fn drain_credential_revision(&self, account_key: &str, revision: &str) -> usize {
        let mut guard = self.inner.lock().await;
        let before = guard.len();
        guard.retain(|key, _| {
            !(key.account_key == account_key && key.credential_revision == revision)
        });
        let removed = before - guard.len();
        self.metrics
            .pool_evictions_total
            .increment("credential_revision");
        self.metrics
            .pool_entries
            .store(guard.len() as u64, std::sync::atomic::Ordering::Relaxed);
        removed
    }

    /// 当前条目数（观测）。
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// 是否为空（clippy len-without-is-empty）。
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }
}

/// 路由类别（与官方 ClientRouteClass 对齐的低基数标签）。
pub fn route_class_label(operation: Operation) -> &'static str {
    match operation.route_class() {
        codex_http_client::ClientRouteClass::Api => "api",
        codex_http_client::ClientRouteClass::WebSocket => "wss",
        codex_http_client::ClientRouteClass::Auth => "auth",
        codex_http_client::ClientRouteClass::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(account: &str, revision: &str) -> ClientPoolKey {
        ClientPoolKey::new(account, revision, ProxyDigest::new("digest"), "api")
    }

    #[tokio::test]
    async fn pool_isolates_accounts_and_revisions() {
        let pool = ClientPool::new(std::sync::Arc::new(Metrics::default()));
        let c1 = pool
            .get_or_insert_with(key("account-a", "rev-1"), || Ok(reqwest::Client::new()))
            .await
            .expect("build");
        let c2 = pool
            .get_or_insert_with(key("account-a", "rev-1"), || {
                panic!("must reuse pooled client")
            })
            .await
            .expect("reuse");
        // 第二次 get_or_insert 未调用 build 闭包（panic 即失败）——
        // 同一 key 必须复用同一 client 实例（连接/凭据隔离）。
        let _ = (c1, c2);
        assert_eq!(pool.len().await, 1);
    }

    #[tokio::test]
    async fn different_revision_gets_new_client() {
        let pool = ClientPool::new(std::sync::Arc::new(Metrics::default()));
        let _ = pool
            .get_or_insert_with(key("account-a", "rev-1"), || Ok(reqwest::Client::new()))
            .await
            .expect("build");
        let _ = pool
            .get_or_insert_with(key("account-a", "rev-2"), || Ok(reqwest::Client::new()))
            .await
            .expect("build");
        assert_eq!(pool.len().await, 2);
    }

    #[tokio::test]
    async fn drain_credential_revision_scoped_to_account() {
        let pool = ClientPool::new(std::sync::Arc::new(Metrics::default()));
        let _ = pool
            .get_or_insert_with(key("account-a", "rev-1"), || Ok(reqwest::Client::new()))
            .await
            .expect("build");
        let _ = pool
            .get_or_insert_with(key("account-a", "rev-2"), || Ok(reqwest::Client::new()))
            .await
            .expect("build");
        let _ = pool
            .get_or_insert_with(key("account-b", "rev-1"), || Ok(reqwest::Client::new()))
            .await
            .expect("build");
        // F-014：drain 按 account_key + revision 作用域——
        // account-a/rev-1 只清自己的，account-b/rev-1 不受影响。
        let removed = pool.drain_credential_revision("account-a", "rev-1").await;
        assert_eq!(removed, 1);
        assert_eq!(pool.len().await, 2);
        let removed = pool.drain_credential_revision("account-b", "rev-1").await;
        assert_eq!(removed, 1);
        assert_eq!(pool.len().await, 1);
    }

    #[tokio::test]
    async fn cache_hit_refreshes_last_used() {
        // F-014：活跃 hit 不得被"创建后 5 分钟"TTL 淘汰。
        let pool = ClientPool::new(std::sync::Arc::new(Metrics::default()));
        let client = pool
            .get_or_insert_with(key("account-a", "rev-1"), || Ok(reqwest::Client::new()))
            .await
            .expect("build");
        // 反复 hit（模拟活跃流量）应保持同一 client 且不触发重建：
        // build 闭包 panic 即证明发生重建。
        for _ in 0..10 {
            let _ = pool
                .get_or_insert_with(key("account-a", "rev-1"), || panic!("活跃 client 不得重建"))
                .await
                .expect("hit");
        }
        assert_eq!(pool.len().await, 1);
        let _ = client;
    }
}
