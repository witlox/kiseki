//! Connection pooling for transports.
//!
//! `ConnectionPool` maintains a per-endpoint pool of idle connections,
//! returning them on demand and creating new ones when needed. Connections
//! are returned to the pool on `PooledConn` drop rather than being closed.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::error::TransportError;
use crate::traits::Transport;

/// Configuration for a connection pool.
#[derive(Clone, Copy, Debug)]
pub struct PoolConfig {
    /// Maximum idle connections per endpoint. Default: 4.
    pub max_idle: usize,
    /// Maximum total connections per endpoint. Default: 8.
    pub max_per_endpoint: usize,
    /// Idle connections older than this are evicted. Default: 30s.
    pub idle_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_idle: 4,
            max_per_endpoint: 8,
            idle_timeout: Duration::from_secs(30),
        }
    }
}

/// A timestamped idle connection.
struct IdleConn<C> {
    conn: C,
    returned_at: Instant,
}

/// Per-endpoint connection pool for a given `Transport`.
///
/// Thread-safe via internal `tokio::sync::Mutex`. The pool tracks both
/// idle connections (available for reuse) and total active connections
/// per endpoint to enforce `max_per_endpoint`.
pub struct ConnectionPool<T: Transport> {
    transport: T,
    config: PoolConfig,
    /// Idle connections keyed by endpoint address.
    idle: tokio::sync::Mutex<HashMap<SocketAddr, VecDeque<IdleConn<T::Conn>>>>,
    /// Count of active (checked-out) connections per endpoint.
    active: tokio::sync::Mutex<HashMap<SocketAddr, usize>>,
}

impl<T: Transport> ConnectionPool<T> {
    /// Create a new pool wrapping the given transport.
    pub fn new(transport: T, config: PoolConfig) -> Self {
        Self {
            transport,
            config,
            idle: tokio::sync::Mutex::new(HashMap::new()),
            active: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Get a connection to `addr`, reusing an idle one if available.
    pub async fn get(&self, addr: SocketAddr) -> Result<T::Conn, TransportError> {
        // Try to get an idle connection.
        {
            let mut idle = self.idle.lock().await;
            if let Some(queue) = idle.get_mut(&addr) {
                let now = Instant::now();
                // Evict expired entries from the front.
                while let Some(front) = queue.front() {
                    if now.duration_since(front.returned_at) > self.config.idle_timeout {
                        queue.pop_front();
                    } else {
                        break;
                    }
                }
                // Pop a valid idle connection.
                if let Some(idle_conn) = queue.pop_back() {
                    let mut active = self.active.lock().await;
                    *active.entry(addr).or_insert(0) += 1;
                    return Ok(idle_conn.conn);
                }
            }
        }

        // Check active count before creating new.
        // Lock ordering: idle THEN active (same order as the idle-check block
        // above) to prevent deadlocks.
        {
            let idle = self.idle.lock().await;
            let idle_count = idle.get(&addr).map_or(0, VecDeque::len);
            let active = self.active.lock().await;
            let count = active.get(&addr).copied().unwrap_or(0);
            if count + idle_count >= self.config.max_per_endpoint {
                return Err(TransportError::PoolExhausted(addr.to_string()));
            }
        }

        // Create new connection.
        let conn = self.transport.connect(addr).await?;
        {
            let mut active = self.active.lock().await;
            *active.entry(addr).or_insert(0) += 1;
        }
        Ok(conn)
    }

    /// Return a connection to the pool for reuse.
    ///
    /// If the pool is full for this endpoint, the connection is dropped.
    pub async fn put(&self, addr: SocketAddr, conn: T::Conn) {
        // Decrement active count.
        {
            let mut active = self.active.lock().await;
            if let Some(count) = active.get_mut(&addr) {
                *count = count.saturating_sub(1);
            }
        }

        let mut idle = self.idle.lock().await;
        let queue = idle.entry(addr).or_insert_with(VecDeque::new);
        if queue.len() < self.config.max_idle {
            queue.push_back(IdleConn {
                conn,
                returned_at: Instant::now(),
            });
        }
        // else: drop the connection (pool full for this endpoint)
    }

    /// Evict all idle connections that have exceeded `idle_timeout`.
    pub async fn evict_expired(&self) {
        let now = Instant::now();
        let mut idle = self.idle.lock().await;
        for queue in idle.values_mut() {
            while let Some(front) = queue.front() {
                if now.duration_since(front.returned_at) > self.config.idle_timeout {
                    queue.pop_front();
                } else {
                    break;
                }
            }
        }
    }

    /// Number of idle connections across all endpoints.
    pub async fn idle_count(&self) -> usize {
        let idle = self.idle.lock().await;
        idle.values().map(VecDeque::len).sum()
    }

    /// Number of active (checked-out) connections across all endpoints.
    pub async fn active_count(&self) -> usize {
        let active = self.active.lock().await;
        active.values().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{Connection, PeerIdentity, Transport};
    use kiseki_common::ids::OrgId;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    /// Mock connection for testing.
    struct MockConn {
        remote: SocketAddr,
        identity: PeerIdentity,
    }

    impl MockConn {
        fn new(addr: SocketAddr) -> Self {
            Self {
                remote: addr,
                identity: PeerIdentity {
                    org_id: OrgId(uuid::Uuid::nil()),
                    common_name: "test".into(),
                    cert_fingerprint: [0u8; 32],
                },
            }
        }
    }

    impl Connection for MockConn {
        fn peer_identity(&self) -> &PeerIdentity {
            &self.identity
        }
        fn remote_addr(&self) -> SocketAddr {
            self.remote
        }
    }

    impl AsyncRead for MockConn {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for MockConn {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Mock transport that creates `MockConn` instances.
    #[derive(Debug)]
    struct MockTransport;

    impl Transport for MockTransport {
        type Conn = MockConn;
        async fn connect(&self, addr: SocketAddr) -> Result<MockConn, TransportError> {
            Ok(MockConn::new(addr))
        }
        fn name(&self) -> &'static str {
            "mock"
        }
    }

    #[tokio::test]
    async fn pool_reuses_connection() {
        let pool = ConnectionPool::new(MockTransport, PoolConfig::default());
        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();

        // Get a connection, return it, get again — should reuse.
        let conn = pool.get(addr).await.unwrap();
        assert_eq!(pool.active_count().await, 1);
        pool.put(addr, conn).await;
        assert_eq!(pool.idle_count().await, 1);
        assert_eq!(pool.active_count().await, 0);

        let _conn2 = pool.get(addr).await.unwrap();
        assert_eq!(pool.idle_count().await, 0); // pulled from idle
        assert_eq!(pool.active_count().await, 1);
    }

    #[tokio::test]
    async fn pool_enforces_max_per_endpoint() {
        let config = PoolConfig {
            max_idle: 2,
            max_per_endpoint: 2,
            idle_timeout: Duration::from_secs(30),
        };
        let pool = ConnectionPool::new(MockTransport, config);
        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();

        let _c1 = pool.get(addr).await.unwrap();
        let _c2 = pool.get(addr).await.unwrap();
        let result = pool.get(addr).await;
        assert!(matches!(result, Err(TransportError::PoolExhausted(_))));
    }

    #[tokio::test]
    async fn pool_evicts_expired_idle() {
        let config = PoolConfig {
            max_idle: 4,
            max_per_endpoint: 8,
            idle_timeout: Duration::from_millis(50),
        };
        let pool = ConnectionPool::new(MockTransport, config);
        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();

        let conn = pool.get(addr).await.unwrap();
        pool.put(addr, conn).await;
        assert_eq!(pool.idle_count().await, 1);

        // Wait for expiry.
        tokio::time::sleep(Duration::from_millis(100)).await;
        pool.evict_expired().await;
        assert_eq!(pool.idle_count().await, 0);
    }

    #[tokio::test]
    async fn pool_drops_excess_idle() {
        let config = PoolConfig {
            max_idle: 1,
            max_per_endpoint: 8,
            idle_timeout: Duration::from_secs(30),
        };
        let pool = ConnectionPool::new(MockTransport, config);
        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();

        let c1 = pool.get(addr).await.unwrap();
        let c2 = pool.get(addr).await.unwrap();
        pool.put(addr, c1).await;
        pool.put(addr, c2).await;
        // max_idle=1, so only 1 should be kept.
        assert_eq!(pool.idle_count().await, 1);
    }
}
