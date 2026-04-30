//! Non-blocking reverse-DNS resolver with a 5-minute cache.
//!
//! The public API is deliberately synchronous from the caller's perspective:
//! `lookup_or_enqueue` returns `Some(domain)` immediately from the cache, or
//! `None` while enqueuing a background PTR lookup.  On the next render tick
//! (≤ 400 ms) the cache will be populated and the caller gets the name.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hickory_resolver::TokioAsyncResolver;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use tokio::sync::mpsc;

const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

struct CacheEntry {
    domain: Option<String>, // None = negative / NXDOMAIN
    fetched_at: Instant,
}

impl CacheEntry {
    fn expired(&self) -> bool {
        self.fetched_at.elapsed() > CACHE_TTL
    }
}

struct Inner {
    cache: HashMap<IpAddr, CacheEntry>,
    queue_tx: mpsc::UnboundedSender<IpAddr>,
}

/// Shared non-blocking reverse-DNS resolver.
///
/// Cheaply cloneable (`Arc` inside).  Safe to call from the render loop.
#[derive(Clone)]
pub struct DnsResolver {
    inner: Arc<Mutex<Inner>>,
}

impl DnsResolver {
    /// Creates the resolver and spawns the background worker on `rt`.
    pub fn new(rt: &tokio::runtime::Handle) -> Self {
        let (queue_tx, queue_rx) = mpsc::unbounded_channel::<IpAddr>();

        let inner = Arc::new(Mutex::new(Inner {
            cache: HashMap::new(),
            queue_tx,
        }));

        let worker_inner = inner.clone();
        rt.spawn(async move {
            worker(queue_rx, worker_inner).await;
        });

        DnsResolver { inner }
    }

    /// Returns `Some(domain)` if a PTR result is cached, `None` otherwise.
    ///
    /// On a cache miss this enqueues a lookup; subsequent calls will return
    /// the result once the background worker finishes.
    pub fn lookup_or_enqueue(&self, ip: IpAddr) -> Option<String> {
        let mut guard = self.inner.lock().expect("dns resolver lock not poisoned");

        if let Some(entry) = guard.cache.get(&ip) {
            if !entry.expired() {
                return entry.domain.clone();
            }
            // Stale entry — remove it and re-enqueue below.
            guard.cache.remove(&ip);
        }

        // Miss — enqueue. Ignore send error (worker might have exited; no
        // panic, just no DNS for that IP).
        let _ = guard.queue_tx.send(ip);
        None
    }
}

async fn worker(
    mut rx: mpsc::UnboundedReceiver<IpAddr>,
    inner: Arc<Mutex<Inner>>,
) {
    let resolver = TokioAsyncResolver::tokio(
        ResolverConfig::default(),
        ResolverOpts::default(),
    );

    // Deduplicate — track IPs that are currently in-flight so we don't
    // spawn a new lookup for every poll tick while a query is pending.
    let mut inflight: std::collections::HashSet<IpAddr> = std::collections::HashSet::new();

    // Use a JoinSet so we can run up to N parallel PTR queries without blocking.
    let mut join_set: tokio::task::JoinSet<(IpAddr, Option<String>)> = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            // New IP to look up.
            Some(ip) = rx.recv() => {
                if inflight.contains(&ip) { continue; }
                // Check cache again — another task may have resolved it already.
                {
                    let guard = inner.lock().expect("lock not poisoned");
                    if guard.cache.contains_key(&ip) { continue; }
                }
                inflight.insert(ip);

                let resolver_clone = resolver.clone();
                join_set.spawn(async move {
                    let domain = resolver_clone.reverse_lookup(ip).await.ok()
                        .and_then(|rl| {
                            rl.into_iter().next().map(|name| {
                                // Strip trailing dot if present.
                                let s = name.to_string();
                                s.trim_end_matches('.').to_owned()
                            })
                        });
                    (ip, domain)
                });
            }

            // Completed lookup — update cache.
            Some(Ok((ip, domain))) = join_set.join_next() => {
                inflight.remove(&ip);
                let mut guard = inner.lock().expect("lock not poisoned");
                guard.cache.insert(ip, CacheEntry { domain, fetched_at: Instant::now() });
            }

            else => {
                // Channel closed — exit worker.
                break;
            }
        }
    }
}
