//! [`DnsForwarder`] — local UDP DNS server that tunnels queries via DoH.
//!
//! ## How it works
//!
//! 1. Binds a UDP socket on `config.listen_addr` (default `127.0.0.1:5353`).
//! 2. Receives raw DNS wire-format query packets from local clients.
//! 3. POSTs each query to a DoH upstream (`config.doh_url`) using `reqwest`
//!    with a SOCKS5 proxy pointed at the local mirage-client SOCKS5 server.
//!    This causes the DoH HTTPS request to travel through the Mirage chain,
//!    preventing DNS leaks.
//! 4. Returns the DoH response bytes to the original sender as a UDP reply.
//!
//! A simple TTL-bounded in-memory cache avoids redundant upstream queries for
//! identical wire-format requests.  Cache entries expire after
//! `config.cache_ttl_secs` seconds regardless of the DNS TTL in the response.
//!
//! ## DNS transaction IDs
//!
//! DNS wire format starts with a 2-byte transaction ID (`[0..2]`).  The cache
//! key strips the ID so that the same question with different IDs still hits
//! the cache.  The cached response ID is patched to match the incoming query
//! before sending back to the caller.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::Instant,
};

use mirage_core::{config::DnsConfig, error::MirageError};
use tokio::{net::UdpSocket, sync::RwLock};
use tracing::{debug, error, info, warn};

// ── Cache ─────────────────────────────────────────────────────────────────────

struct CacheEntry {
    /// Raw DNS wire response bytes (with the transaction ID from the stored
    /// query — callers patch it before sending).
    response: Vec<u8>,
    expires_at: Instant,
}

/// Cache key: DNS query wire bytes with the 2-byte transaction ID zeroed out.
fn cache_key(query: &[u8]) -> Vec<u8> {
    if query.len() < 2 {
        return query.to_vec();
    }
    let mut key = query.to_vec();
    key[0] = 0;
    key[1] = 0;
    key
}

// ── DnsForwarder ──────────────────────────────────────────────────────────────

/// Local DoH forwarder.  Construct via [`DnsForwarder::new`] and drive with
/// [`DnsForwarder::run`].
pub struct DnsForwarder {
    config: DnsConfig,
    cache: Arc<RwLock<HashMap<Vec<u8>, CacheEntry>>>,
    /// Shared reqwest client configured with SOCKS5 proxy.
    client: reqwest::Client,
}

impl DnsForwarder {
    /// Build a forwarder from the given config.
    ///
    /// Creates the internal reqwest client with the SOCKS5 proxy configured.
    /// Returns an error if the proxy URL or DoH URL is malformed.
    pub fn new(config: DnsConfig) -> Result<Self, MirageError> {
        let proxy = reqwest::Proxy::all(&config.socks_addr)
            .map_err(|e| MirageError::Config(format!("dns socks proxy invalid: {e}")))?;

        let client = reqwest::Client::builder()
            .proxy(proxy)
            .build()
            .map_err(|e| MirageError::Config(format!("dns reqwest client build failed: {e}")))?;

        Ok(Self {
            config,
            cache: Arc::new(RwLock::new(HashMap::new())),
            client,
        })
    }

    /// Bind the UDP socket and serve DNS queries until an error occurs or the
    /// future is cancelled.
    pub async fn run(self: Arc<Self>) -> Result<(), MirageError> {
        let listen: SocketAddr = self.config.listen_addr.parse().map_err(|e| {
            MirageError::Config(format!(
                "dns listen_addr {:?} is invalid: {e}",
                self.config.listen_addr
            ))
        })?;

        let socket = Arc::new(
            UdpSocket::bind(listen)
                .await
                .map_err(|e| MirageError::Config(format!("dns bind {listen}: {e}")))?,
        );

        info!(addr = %listen, doh_url = %self.config.doh_url, "dns forwarder listening");

        // Reuse a fixed-size recv buffer.  DNS queries are at most 512 bytes
        // for UDP (RFC 1035 §2.3.4) but allow 4096 for EDNS0 (RFC 2671).
        let mut buf = vec![0u8; 4096];

        loop {
            let (len, peer) = socket.recv_from(&mut buf).await?;
            let query = buf[..len].to_vec();
            let forwarder = Arc::clone(&self);
            let socket_clone = Arc::clone(&socket);

            // Spawn a task per query so slow upstreams don't stall the receive loop.
            tokio::spawn(async move {
                match forwarder.resolve(&query).await {
                    Ok(mut response) => {
                        // Patch the response transaction ID to match the query.
                        if query.len() >= 2 && response.len() >= 2 {
                            response[0] = query[0];
                            response[1] = query[1];
                        }
                        if let Err(e) = socket_clone.send_to(&response, peer).await {
                            warn!(peer = %peer, error = %e, "dns: send_to failed");
                        }
                    }
                    Err(e) => {
                        error!(peer = %peer, error = %e, "dns: resolve failed, dropping query");
                    }
                }
            });
        }
    }

    /// Resolve a single DNS query wire packet, using the cache when possible.
    async fn resolve(&self, query: &[u8]) -> Result<Vec<u8>, MirageError> {
        if query.len() < 12 {
            return Err(MirageError::Protocol(format!(
                "dns query too short: {} bytes",
                query.len()
            )));
        }

        let key = cache_key(query);

        // ── Cache read ────────────────────────────────────────────────────────
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.get(&key) {
                if entry.expires_at > Instant::now() {
                    debug!("dns cache hit");
                    return Ok(entry.response.clone());
                }
            }
        }

        // ── DoH upstream request ──────────────────────────────────────────────
        debug!(doh_url = %self.config.doh_url, "dns: forwarding via DoH");

        let response = self
            .client
            .post(&self.config.doh_url)
            .header("Content-Type", "application/dns-message")
            .header("Accept", "application/dns-message")
            .body(query.to_vec())
            .send()
            .await
            .map_err(|e| MirageError::Protocol(format!("dns DoH request failed: {e}")))?;

        if !response.status().is_success() {
            return Err(MirageError::Protocol(format!(
                "dns DoH upstream returned HTTP {}",
                response.status()
            )));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| MirageError::Protocol(format!("dns DoH read body failed: {e}")))?;

        let response_wire = bytes.to_vec();

        // ── Cache write ───────────────────────────────────────────────────────
        {
            let mut cache = self.cache.write().await;
            // Evict stale entries opportunistically to keep memory bounded.
            let now = Instant::now();
            cache.retain(|_, v| v.expires_at > now);

            cache.insert(
                key,
                CacheEntry {
                    response: response_wire.clone(),
                    expires_at: now
                        + std::time::Duration::from_secs(self.config.cache_ttl_secs),
                },
            );
        }

        Ok(response_wire)
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a minimal 12-byte DNS query with the given transaction ID.
    fn make_query(txid: u16) -> Vec<u8> {
        let mut q = vec![0u8; 12];
        q[0] = (txid >> 8) as u8;
        q[1] = (txid & 0xff) as u8;
        // QR=0 (query), OPCODE=0 (QUERY), flags=0, QDCOUNT=0
        q
    }

    #[test]
    fn test_cache_key_zeroes_txid() {
        let q1 = make_query(0x1234);
        let q2 = make_query(0xabcd);
        // Same body, different IDs → same key
        assert_eq!(cache_key(&q1), cache_key(&q2));
    }

    #[test]
    fn test_cache_key_preserves_body() {
        let mut q1 = make_query(0x0001);
        let mut q2 = make_query(0x0001);
        q1[5] = 0x01; // QDCOUNT lo-byte differs
        q2[5] = 0x02;
        assert_ne!(cache_key(&q1), cache_key(&q2));
    }

    #[test]
    fn test_cache_key_short_query() {
        // Should not panic on very short input
        let key = cache_key(&[0xAB]);
        assert_eq!(key, vec![0xAB]);
    }

    /// Build a DnsForwarder with a deliberately invalid proxy URL and verify
    /// that `new()` returns a Config error rather than panicking.
    #[test]
    fn test_new_invalid_proxy_errors() {
        let mut config = DnsConfig::default();
        config.socks_addr = "not a valid url !!".to_owned();
        let result = DnsForwarder::new(config);
        assert!(
            matches!(result, Err(MirageError::Config(_))),
            "expected Config error for bad proxy URL"
        );
    }

    /// Verify that a forwarder rejects a query shorter than the minimum 12-byte
    /// DNS header without panicking.
    #[tokio::test]
    async fn test_resolve_rejects_short_query() {
        let config = DnsConfig::default();
        // Use a proxy URL that parses successfully but will never connect.
        // We just need the client to be constructed; resolve will fail at the
        // length check before making any network call.
        let mut cfg = config.clone();
        cfg.socks_addr = "socks5h://127.0.0.1:19999".to_owned();
        let forwarder = DnsForwarder::new(cfg).expect("should construct");

        let result = forwarder.resolve(&[0u8; 4]).await;
        assert!(
            matches!(result, Err(MirageError::Protocol(_))),
            "expected Protocol error for short query"
        );
    }
}
